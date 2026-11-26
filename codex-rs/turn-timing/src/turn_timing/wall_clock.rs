//! Derived wall-clock profile of one terminal turn record.
//!
//! Every value comes from evidence already frozen in the same `TurnTiming`:
//! the activity timeline, blocking waits, the native dependency graph,
//! request/tool rows, recovery telemetry and the accepted-final tail. This
//! projection never changes scheduling, adds a timer or fills a missing
//! endpoint. Operation endpoints are millisecond offsets; timeline and graph
//! values are nanoseconds on the same turn clock. Unions are not additive with
//! the exclusive partition, and no value is a proven saving or causal verdict.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::TurnTimingModelRequest;
use codex_protocol::protocol::TurnTimingToolCall;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_protocol::timing::TurnTimingBlockingCategory;
use codex_protocol::timing::TurnTimingDependencyGraph;
use codex_protocol::timing::TurnTimingDependencyNode as Node;
use codex_protocol::timing::TurnTimingDependencyNodeKind as Kind;
use serde_json::Value;
use serde_json::json;

use super::ToolActionClass;
use super::dependency::request_end;
use super::dependency::tool_owner_end;
use super::tool_action_class;

const SCHEMA_VERSION: u16 = 1;
const MAX_CLASS_ROWS: usize = 32;
const MAX_QUEUE_ROWS: usize = 32;
const MAX_DELEGATION_ROWS: usize = 64;
const MAX_PATH_ROWS: usize = 8;
const MAX_REASONS: usize = 16;
const MAX_CLASS_NAME_BYTES: usize = 96;
const COMPARISON: &str = "kdr tools diagnose wall-clock-profile --rollout CANDIDATE --baseline-rollout BASELINE";

type Span = (u64, u64);

fn ns(ms: Option<u64>) -> Option<u64> {
    ms?.checked_mul(1_000_000)
}

fn span(start: Option<u64>, end: Option<u64>) -> Option<Span> {
    let (start, end) = (start?, end?);
    (start <= end).then_some((start, end))
}

/// Sorted, disjoint, nonempty spans.
fn merged(mut spans: Vec<Span>) -> Vec<Span> {
    spans.retain(|(start, end)| start < end);
    spans.sort_unstable();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

fn length(spans: &[Span]) -> u64 {
    spans
        .iter()
        .fold(0u64, |sum, (start, end)| sum.saturating_add(end.saturating_sub(*start)))
}

/// Intersection of two merged span lists.
fn intersect(a: &[Span], b: &[Span]) -> Vec<Span> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        let start = a[i].0.max(b[j].0);
        let end = a[i].1.min(b[j].1);
        if start < end {
            out.push((start, end));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Parts of merged `a` not covered by merged `b`.
fn subtract(a: &[Span], b: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut j = 0;
    for &(start, end) in a {
        let mut cursor = start;
        while j < b.len() && b[j].1 <= cursor {
            j += 1;
        }
        let mut k = j;
        while k < b.len() && b[k].0 < end {
            if b[k].0 > cursor {
                out.push((cursor, b[k].0));
            }
            cursor = cursor.max(b[k].1);
            k += 1;
        }
        if cursor < end {
            out.push((cursor, end));
        }
    }
    out
}

/// Peak concurrency plus time covered by exactly one and by at least two spans.
fn concurrency(spans: &[Span]) -> (u32, Vec<Span>, Vec<Span>) {
    let mut events = Vec::with_capacity(spans.len() * 2);
    for &(start, end) in spans.iter().filter(|(start, end)| start < end) {
        events.push((start, 1i64));
        events.push((end, -1i64));
    }
    // Ends sort before starts at equal times, so touching spans never overlap.
    events.sort_unstable();
    let (mut active, mut peak, mut previous) = (0i64, 0i64, 0u64);
    let (mut single, mut multi) = (Vec::new(), Vec::new());
    for (at, delta) in events {
        if at > previous {
            if active == 1 {
                single.push((previous, at));
            } else if active >= 2 {
                multi.push((previous, at));
            }
        }
        previous = at;
        active += delta;
        peak = peak.max(active);
    }
    (u32::try_from(peak).unwrap_or(u32::MAX), merged(single), merged(multi))
}

fn ratio(numerator: u64, denominator: u64) -> Value {
    if denominator == 0 {
        Value::Null
    } else {
        json!(numerator as f64 / denominator as f64)
    }
}

fn summary(spans: &[Span]) -> Value {
    json!({
        "count": spans.len(),
        "sumNs": length(spans),
        "unionNs": length(&merged(spans.to_vec())),
        "maxNs": spans.iter().map(|(start, end)| end - start).max().unwrap_or(0),
    })
}

fn status(gaps: &BTreeSet<&'static str>) -> &'static str {
    if gaps.is_empty() { "available" } else { "partial" }
}

fn reasons(gaps: &BTreeSet<&'static str>) -> Vec<&'static str> {
    gaps.iter().take(MAX_REASONS).copied().collect()
}

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}

fn class_name(name: &str) -> String {
    let mut end = name.len().min(MAX_CLASS_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("tool:{}", &name[..end])
}

struct ToolOperation<'a> {
    call: &'a TurnTimingToolCall,
    envelope: Option<Span>,
    execution: Option<Span>,
    dispatch_wait: Option<Span>,
    children: Vec<Span>,
    has_children: bool,
}

struct Operations<'a> {
    models: Vec<(&'a TurnTimingModelRequest, Span)>,
    tools: Vec<ToolOperation<'a>>,
    sampling_ready: Vec<Span>,
    gaps: BTreeSet<&'static str>,
}

/// Nested release boundaries keyed by (cell, runtime call), built once per turn.
/// Same rule as the graph's per-call lookup: the first matching row decides.
fn nested_releases(timing: &TurnTiming) -> HashMap<(Option<&str>, Option<&str>), Option<u64>> {
    let mut index = HashMap::new();
    let rows = timing
        .measurement_evidence
        .as_ref()
        .and_then(|evidence| evidence.get("nestedDependencies"))
        .and_then(|packet| packet.get("rows"))
        .and_then(Value::as_array);
    for row in rows.into_iter().flatten() {
        index
            .entry((row["cellId"].as_str(), row["runtimeCallId"].as_str()))
            .or_insert_with(|| row.get("releasedAtMs").and_then(Value::as_u64));
    }
    index
}

impl<'a> Operations<'a> {
    fn collect(timing: &'a TurnTiming, duration: u64) -> Self {
        let clip = |(start, end): Span| (start.min(duration), end.min(duration));
        let releases = nested_releases(timing);
        let mut gaps = BTreeSet::new();
        let mut models = Vec::new();
        for request in &timing.model_requests {
            match span(ns(request.dispatch_ms), ns(request_end(timing, request))) {
                Some(envelope) => models.push((request, clip(envelope))),
                // A request that never reached transport entry is not an execution.
                None if request.dispatch_ms.is_some() => {
                    gaps.insert("model_request_endpoint_missing");
                }
                None => {}
            }
        }
        let mut tools = timing
            .tool_calls
            .iter()
            .map(|call| {
                let release = match call.source {
                    TurnTimingToolCallSource::Direct => call.delivered_at_ms,
                    TurnTimingToolCallSource::CodeMode => releases
                        .get(&(call.parent_cell_id.as_deref(), call.runtime_tool_call_id.as_deref()))
                        .copied()
                        .flatten(),
                };
                let end = release.or_else(|| tool_owner_end(timing, call));
                let envelope = span(ns(call.accepted_at_ms), ns(end)).map(clip);
                if envelope.is_none() {
                    gaps.insert("tool_envelope_endpoint_missing");
                }
                let execution =
                    span(ns(call.handler_entry_at_ms), ns(call.handler_exit_at_ms)).map(clip);
                if execution.is_none() && call.handler_entry_at_ms.is_some() {
                    gaps.insert("tool_execution_endpoint_missing");
                }
                ToolOperation {
                    call,
                    envelope,
                    execution,
                    dispatch_wait: span(ns(call.accepted_at_ms), ns(call.handler_entry_at_ms))
                        .map(clip),
                    children: Vec::new(),
                    has_children: false,
                }
            })
            .collect::<Vec<_>>();
        // Nested calls name the model-visible call ID of their direct parent.
        let mut parents: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, operation) in tools.iter().enumerate() {
            if operation.call.source == TurnTimingToolCallSource::Direct {
                parents
                    .entry(operation.call.call_id.as_str())
                    .or_default()
                    .push(index);
            }
        }
        let mut links = Vec::new();
        for (index, operation) in tools.iter().enumerate() {
            if operation.call.source != TurnTimingToolCallSource::CodeMode {
                continue;
            }
            let Some(parent) = operation.call.parent_call_id.as_deref() else {
                continue;
            };
            match parents.get(parent).map(Vec::as_slice) {
                Some([parent]) => links.push((*parent, index)),
                _ => {
                    gaps.insert("nested_parent_ambiguous_or_missing");
                }
            }
        }
        for (parent, child) in links {
            tools[parent].has_children = true;
            if let Some(envelope) = tools[child].envelope {
                tools[parent].children.push(envelope);
            }
        }
        // One continuation dispatch is shared by every result it resumed.
        let mut dispatches = BTreeMap::new();
        for call in &timing.tool_calls {
            if let (Some(resumed), Some(wait)) =
                (ns(call.model_resumed_at_ms), call.ready_to_sample_to_dispatch_ns)
            {
                dispatches.entry(resumed).or_insert(wait);
            }
        }
        let sampling_ready = dispatches
            .into_iter()
            .map(|(resumed, wait)| clip((resumed.saturating_sub(wait), resumed)))
            .collect();
        Self {
            models,
            tools,
            sampling_ready,
            gaps,
        }
    }

    /// Model physical attempts and tool handler executions without nested children.
    fn leaf_executions(&self) -> Vec<Span> {
        self.models
            .iter()
            .map(|(_, envelope)| *envelope)
            .chain(
                self.tools
                    .iter()
                    .filter(|operation| !operation.has_children)
                    .filter_map(|operation| operation.execution),
            )
            .collect()
    }

    /// Accepted-but-not-entered tools and ready-but-not-dispatched sampling.
    fn ready_waits(&self) -> Vec<Span> {
        self.tools
            .iter()
            .filter_map(|operation| operation.dispatch_wait)
            .chain(self.sampling_ready.iter().copied())
            .collect()
    }
}

struct PathAnalysis<'a> {
    path: Vec<&'a Node>,
    /// Composition label of each path node, parallel to `path`.
    labels: Vec<&'static str>,
    elapsed: u64,
    model: u64,
    tool: u64,
    nested_cell: u64,
    wait: u64,
    handoff: u64,
    retry_handoff: u64,
    execution_span: u64,
    execution_work: u64,
    contiguous: bool,
}

impl PathAnalysis<'_> {
    fn execution(&self) -> u64 {
        self.model.saturating_add(self.tool)
    }
}

fn node_duration(node: &Node) -> Option<u64> {
    node.end_ns?.checked_sub(node.start_ns?)
}

/// A Code Mode cell that emitted nested calls overlaps their nodes; its
/// segments are orchestration envelopes, not additional execution work.
fn nested_cell(node: &Node, parents: &BTreeSet<&str>) -> bool {
    node.kind == Kind::Tool
        && parents.contains(node.id.split(":segment-").next().unwrap_or_default())
}

fn analyze_graph<'a>(
    graph: Option<&'a TurnTimingDependencyGraph>,
    parents: &BTreeSet<&str>,
) -> Result<PathAnalysis<'a>, Value> {
    let Some(graph) = graph else {
        return Err(unavailable("dependency_graph_not_recorded"));
    };
    if !graph.complete {
        let graph_reasons = graph
            .unavailable_reasons
            .iter()
            .take(MAX_REASONS)
            .collect::<Vec<_>>();
        return Err(json!({"status": "unavailable", "reason": "dependency_graph_incomplete",
            "graphReasons": graph_reasons}));
    }
    let index: HashMap<&str, &Node> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    if index.len() != graph.nodes.len() {
        return Err(unavailable("duplicate_graph_node_identity"));
    }
    let mut indegree: HashMap<&str, usize> = HashMap::with_capacity(index.len());
    let mut successors: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &graph.nodes {
        if node_duration(node).is_none() {
            return Err(unavailable("graph_node_span_invalid"));
        }
        indegree.entry(node.id.as_str()).or_insert(0);
        for dependency in &node.depends_on {
            if !index.contains_key(dependency.as_str()) {
                return Err(unavailable("unknown_graph_dependency"));
            }
            successors
                .entry(dependency.as_str())
                .or_default()
                .push(node.id.as_str());
            *indegree.entry(node.id.as_str()).or_insert(0) += 1;
        }
    }
    let mut ready = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    let mut order = Vec::with_capacity(index.len());
    while let Some(id) = ready.pop() {
        order.push(id);
        for next in successors.get(id).into_iter().flatten() {
            if let Some(count) = indegree.get_mut(next) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.push(*next);
                }
            }
        }
    }
    if order.len() != index.len() {
        return Err(unavailable("cyclic_task_graph"));
    }
    let execution = |node: &Node| match node.kind {
        Kind::Tool if nested_cell(node, parents) => 0,
        Kind::Model | Kind::Tool => node_duration(node).unwrap_or(0),
        Kind::Boundary | Kind::Wait => 0,
    };
    let mut best: HashMap<&str, u64> = HashMap::with_capacity(order.len());
    let mut execution_work = 0u64;
    for id in &order {
        let node = index[id];
        let weight = execution(node);
        execution_work = execution_work.saturating_add(weight);
        let prior = node
            .depends_on
            .iter()
            .map(|dependency| best.get(dependency.as_str()).copied().unwrap_or(0))
            .max()
            .unwrap_or(0);
        best.insert(*id, prior.saturating_add(weight));
    }
    let Some(finish) = index.get(graph.finish_node.as_str()).copied() else {
        return Err(unavailable("finish_node_not_recorded"));
    };
    // Walk back through each node's binding (latest-finishing) prerequisite.
    let mut path = vec![finish];
    let mut current = finish;
    while !current.depends_on.is_empty() && path.len() <= index.len() {
        let next = current
            .depends_on
            .iter()
            .filter_map(|id| index.get(id.as_str()).copied())
            .max_by(|a, b| {
                a.end_ns
                    .cmp(&b.end_ns)
                    .then(node_duration(a).cmp(&node_duration(b)))
                    .then(b.id.cmp(&a.id))
            });
        let Some(next) = next else {
            break;
        };
        path.push(next);
        current = next;
    }
    if current.id != graph.start_node {
        return Err(unavailable("critical_path_not_rooted_at_start"));
    }
    path.reverse();
    let mut analysis = PathAnalysis {
        path,
        labels: Vec::new(),
        elapsed: 0,
        model: 0,
        tool: 0,
        nested_cell: 0,
        wait: 0,
        handoff: 0,
        retry_handoff: 0,
        execution_span: best.get(finish.id.as_str()).copied().unwrap_or(0),
        execution_work,
        contiguous: true,
    };
    let mut previous_end = None;
    for node in &analysis.path {
        let duration = node_duration(node).unwrap_or(0);
        if previous_end.is_some_and(|end| Some(end) != node.start_ns) {
            analysis.contiguous = false;
        }
        previous_end = node.end_ns;
        analysis.elapsed = analysis.elapsed.saturating_add(duration);
        let (label, bucket) = match node.kind {
            Kind::Model => ("model", &mut analysis.model),
            Kind::Tool if nested_cell(node, parents) => ("nested_cell", &mut analysis.nested_cell),
            Kind::Tool => ("tool", &mut analysis.tool),
            Kind::Wait => ("wait", &mut analysis.wait),
            Kind::Boundary if node.id.contains(":retry-handoff-") => {
                ("retry_handoff", &mut analysis.retry_handoff)
            }
            Kind::Boundary => ("handoff", &mut analysis.handoff),
        };
        *bucket = bucket.saturating_add(duration);
        analysis.labels.push(label);
    }
    Ok(analysis)
}

fn critical_path_duration(path: &Result<PathAnalysis<'_>, Value>) -> Value {
    let analysis = match path {
        Ok(analysis) => analysis,
        Err(reason) => return reason.clone(),
    };
    let mut longest = analysis
        .path
        .iter()
        .zip(&analysis.labels)
        .filter_map(|(node, label)| Some((node_duration(node)?, *node, *label)))
        .filter(|(duration, _, _)| *duration > 0)
        .collect::<Vec<_>>();
    longest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    let longest = longest
        .into_iter()
        .take(MAX_PATH_ROWS)
        .map(|(duration, node, label)| json!({"id": node.id, "kind": label, "durationNs": duration}))
        .collect::<Vec<_>>();
    json!({
        "status": "available",
        "valueNs": analysis.elapsed,
        "executionNs": analysis.execution(),
        "composition": {
            "modelNs": analysis.model,
            "toolNs": analysis.tool,
            "nestedCellNs": analysis.nested_cell,
            "waitNs": analysis.wait,
            "handoffNs": analysis.handoff,
            "retryHandoffNs": analysis.retry_handoff,
        },
        "pathNodeCount": analysis.path.len(),
        "contiguous": analysis.contiguous,
        "longestPathNodes": longest,
    })
}

fn execution_latency(ops: &Operations<'_>, duration: u64, exclusive: bool) -> Value {
    #[derive(Default)]
    struct Class {
        count: u64,
        total: u64,
        max: u64,
    }
    let mut classes: BTreeMap<String, Class> = BTreeMap::new();
    let mut observe = |name: String, value: u64| {
        let class = classes.entry(name).or_default();
        class.count += 1;
        class.total = class.total.saturating_add(value);
        class.max = class.max.max(value);
    };
    for (_, (start, end)) in &ops.models {
        observe("model_request".to_owned(), end - start);
    }
    for operation in &ops.tools {
        let Some((start, end)) = operation.envelope else {
            continue;
        };
        let value = if exclusive {
            let nested = intersect(&merged(operation.children.clone()), &[(start, end)]);
            (end - start).saturating_sub(length(&nested))
        } else {
            end - start
        };
        observe(class_name(&operation.call.tool_name), value);
    }
    let mut rows = classes.into_iter().collect::<Vec<_>>();
    rows.sort_by(|a, b| b.1.total.cmp(&a.1.total).then(a.0.cmp(&b.0)));
    let omitted = rows.split_off(rows.len().min(MAX_CLASS_ROWS));
    let other = omitted.iter().fold(Class::default(), |mut other, (_, class)| {
        other.count += class.count;
        other.total = other.total.saturating_add(class.total);
        other.max = other.max.max(class.max);
        other
    });
    let mut rows = rows
        .into_iter()
        .map(|(name, class)| json!({"class": name, "count": class.count, "totalNs": class.total, "maxNs": class.max}))
        .collect::<Vec<_>>();
    if !omitted.is_empty() {
        rows.push(json!({"class": "other", "classes": omitted.len(), "count": other.count,
            "totalNs": other.total, "maxNs": other.max}));
    }
    let top_level = ops
        .models
        .iter()
        .map(|(_, envelope)| *envelope)
        .chain(
            ops.tools
                .iter()
                .filter(|operation| operation.call.source == TurnTimingToolCallSource::Direct)
                .filter_map(|operation| operation.envelope),
        )
        .collect::<Vec<_>>();
    let covered = length(&merged(top_level));
    let turn = if exclusive { duration.saturating_sub(covered) } else { duration };
    json!({
        "status": status(&ops.gaps),
        "turnNs": turn,
        "classes": rows,
        "gaps": reasons(&ops.gaps),
    })
}

fn share(numerator: u64, exclusive: u64, duration: u64, valid: bool) -> Value {
    if !valid {
        return unavailable("timing_profile_invalid");
    }
    if duration == 0 {
        return unavailable("zero_turn_duration");
    }
    json!({
        "status": "available",
        "ratio": ratio(numerator, duration),
        "numeratorNs": numerator,
        "denominatorNs": duration,
        "exclusiveOnlyRatio": ratio(exclusive, duration),
        "exclusiveOnlyNs": exclusive,
    })
}

fn sum_ms(calls: &[TurnTimingToolCall], field: impl Fn(&TurnTimingToolCall) -> Option<u64>) -> u64 {
    calls
        .iter()
        .filter_map(|call| ns(field(call)))
        .fold(0u64, u64::saturating_add)
}

fn scheduler_latency(ops: &Operations<'_>, timing: &TurnTiming) -> Value {
    let tool_waits = ops
        .tools
        .iter()
        .filter_map(|operation| operation.dispatch_wait)
        .collect::<Vec<_>>();
    let all = merged(ops.ready_waits());
    let calls = timing.tool_calls.as_slice();
    json!({
        "status": status(&ops.gaps),
        "valueNs": length(&all),
        "toolDispatch": summary(&tool_waits),
        "samplingDispatch": summary(&ops.sampling_ready),
        "toolDispatchComponentsNs": {
            "itemToFirstPoll": sum_ms(calls, |call| call.item_to_first_poll_ms),
            "parallelGateWait": sum_ms(calls, |call| call.parallel_gate_wait_ms),
            "authorizationCoordination": sum_ms(calls, |call| call.authorization_state_coordination_ms),
            "preToolHook": sum_ms(calls, |call| call.pre_tool_hook_ms),
            "workspaceEvidenceBefore": sum_ms(calls, |call| call.workspace_evidence_before_ms),
        },
        "gaps": reasons(&ops.gaps),
    })
}

/// Prerequisite, category, clipped span, cancelled and right-censored.
type BlockingRow<'a> = (&'a str, TurnTimingBlockingCategory, Span, bool, bool);

fn blocking_spans(timing: &TurnTiming, duration: u64) -> Option<(Vec<BlockingRow<'_>>, u64)> {
    let blocking = timing.activity_timeline.as_ref()?.blocking.as_ref()?;
    let rows = blocking
        .intervals
        .iter()
        .map(|interval| {
            let start = interval.start_ns.min(duration);
            let end = interval.end_ns.unwrap_or(duration).clamp(start, duration);
            (
                interval.prerequisite.as_str(),
                interval.category,
                (start, end),
                interval.cancelled,
                interval.end_ns.is_none(),
            )
        })
        .collect();
    Some((rows, blocking.dropped_intervals))
}

fn synchronization_wait(timing: &TurnTiming, duration: u64) -> Value {
    let Some((rows, dropped)) = blocking_spans(timing, duration) else {
        return unavailable("blocking_evidence_not_recorded");
    };
    let category = |wanted: TurnTimingBlockingCategory| {
        let selected = rows
            .iter()
            .filter(|row| row.1 == wanted)
            .collect::<Vec<_>>();
        let spans = selected.iter().map(|row| row.2).collect::<Vec<_>>();
        let mut value = summary(&spans);
        value["cancelled"] = json!(selected.iter().filter(|row| row.3).count());
        value["censored"] = json!(selected.iter().filter(|row| row.4).count());
        value
    };
    let all = merged(rows.iter().map(|row| row.2).collect());
    json!({
        "status": if dropped == 0 { "available" } else { "partial" },
        "valueNs": length(&all),
        "lowerBound": dropped != 0,
        "droppedIntervals": dropped,
        "resourceContention": category(TurnTimingBlockingCategory::ResourceContention),
        "dependencyWait": category(TurnTimingBlockingCategory::DependencyWait),
    })
}

fn queueing_attribution(ops: &Operations<'_>, timing: &TurnTiming, duration: u64) -> Value {
    #[derive(Default)]
    struct Row {
        kind: &'static str,
        count: u64,
        sum: u64,
        max: u64,
        spans: Vec<Span>,
        positioned: bool,
        enqueue_wait: Option<u64>,
    }
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    let mut gaps = ops.gaps.clone();
    if let Some((blocking, dropped)) = blocking_spans(timing, duration) {
        if dropped != 0 {
            gaps.insert("blocking_intervals_dropped");
        }
        // Result/interrupt dependency waits are barriers, not queues; they stay
        // in synchronizationWaitTime.dependencyWait.
        for (prerequisite, _, (start, end), _, _) in blocking
            .into_iter()
            .filter(|row| row.1 == TurnTimingBlockingCategory::ResourceContention)
        {
            let row = rows.entry(format!("await:{prerequisite}")).or_insert_with(|| Row {
                kind: "resource_contention",
                positioned: true,
                ..Row::default()
            });
            row.count += 1;
            row.sum = row.sum.saturating_add(end - start);
            row.max = row.max.max(end - start);
            row.spans.push((start, end));
        }
    } else {
        gaps.insert("blocking_evidence_not_recorded");
    }
    let mut tool_row = |name: &str, kind: &'static str, value: Option<u64>, position: Option<Span>| {
        let Some(value) = value.filter(|value| *value > 0) else {
            return;
        };
        let row = rows.entry(name.to_owned()).or_insert_with(|| Row {
            kind,
            positioned: true,
            ..Row::default()
        });
        row.count += 1;
        row.sum = row.sum.saturating_add(value);
        row.max = row.max.max(value);
        match position {
            Some(position) => row.spans.push(position),
            None => row.positioned = false,
        }
    };
    for call in &timing.tool_calls {
        tool_row(
            "tool:first_poll",
            "runtime_dispatch",
            ns(call.item_to_first_poll_ms),
            span(ns(call.accepted_at_ms), ns(call.first_poll_at_ms)),
        );
        tool_row(
            "tool:parallel_gate",
            "admission_policy",
            ns(call.parallel_gate_wait_ms),
            span(ns(call.first_poll_at_ms), ns(call.parallel_gate_admitted_at_ms)),
        );
        tool_row(
            "tool:authorization_coordination",
            "admission_policy",
            ns(call.authorization_state_coordination_ms),
            None,
        );
    }
    for &(start, end) in &ops.sampling_ready {
        tool_row("sampling:ready_to_dispatch", "runtime_dispatch", Some(end - start), Some((start, end)));
    }
    for request in &timing.model_requests {
        for stages in request.response_queue_timing_by_attempt.values() {
            for (stage, queue) in stages {
                let row = rows.entry(format!("response_queue:{stage}")).or_insert_with(|| Row {
                    kind: "host_event_queue",
                    enqueue_wait: Some(0),
                    ..Row::default()
                });
                row.count = row.count.saturating_add(queue.dequeued_events);
                row.sum = row.sum.saturating_add(queue.queue_residence_ns);
                row.max = row.max.max(queue.max_queue_residence_ns);
                row.enqueue_wait = row
                    .enqueue_wait
                    .map(|wait| wait.saturating_add(queue.enqueue_wait_ns));
            }
        }
    }
    let mut rows = rows
        .into_iter()
        .map(|(source, row)| {
            let union = row.positioned.then(|| length(&merged(row.spans.clone())));
            (source, row, union)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| b.1.sum.cmp(&a.1.sum).then(a.0.cmp(&b.0)));
    let omitted = rows.len().saturating_sub(MAX_QUEUE_ROWS);
    // Rank by elapsed coverage; unpositioned sums can overlap other work.
    let dominant = rows
        .iter()
        .filter_map(|(source, _, union)| Some((union.filter(|union| *union > 0)?, source)))
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(a.1)))
        .map(|(_, source)| source.clone());
    let rows = rows
        .into_iter()
        .take(MAX_QUEUE_ROWS)
        .map(|(source, row, union)| {
            json!({"source": source, "kind": row.kind, "count": row.count, "sumNs": row.sum,
                "unionNs": union, "maxNs": row.max, "enqueueWaitNs": row.enqueue_wait})
        })
        .collect::<Vec<_>>();
    json!({
        "status": status(&gaps),
        "dominantSource": dominant,
        "rows": rows,
        "omittedRows": omitted,
        "gaps": reasons(&gaps),
    })
}

fn concurrency_metrics(ops: &Operations<'_>, timing: &TurnTiming, duration: u64) -> (Value, Value, Value, Value, Value) {
    let executions = ops.leaf_executions();
    let total_work = length(&executions);
    let active = merged(executions.clone());
    let union = length(&active);
    let (peak, single, multi) = concurrency(&executions);
    let ready = merged(ops.ready_waits());
    let state = status(&ops.gaps);
    let gaps = reasons(&ops.gaps);
    let efficiency = if peak == 0 {
        unavailable("no_recorded_execution")
    } else {
        let speedup = total_work as f64 / union as f64;
        json!({"status": state, "efficiency": speedup / f64::from(peak), "speedup": speedup,
            "peakConcurrency": peak, "workNs": total_work, "activeUnionNs": union,
            "executions": executions.len(), "gaps": gaps})
    };
    let effective = json!({"status": state, "value": ratio(total_work, duration),
        "activeConcurrency": ratio(total_work, union), "workNs": total_work,
        "turnDurationNs": duration, "activeUnionNs": union, "gaps": gaps});
    let exclusive = &timing.exclusive;
    let machine = exclusive
        .model_only_ns
        .saturating_add(exclusive.tool_only_ns)
        .saturating_add(exclusive.model_plus_tool_ns);
    let overlap = json!({"status": state, "ratio": ratio(length(&multi), union),
        "overlappedNs": length(&multi), "activeUnionNs": union,
        "modelToolOverlapRatio": if timing.profile_valid { ratio(exclusive.model_plus_tool_ns, machine) } else { Value::Null },
        "gaps": gaps});
    // A ready operation cannot depend on unfinished work, so overlap is
    // dependency-safe; resource and policy safety stay unverified.
    let sequential = length(&single);
    let overlappable = length(&intersect(&single, &ready));
    let opportunity = json!({"status": state, "ratio": ratio(overlappable, sequential),
        "overlappableNs": overlappable, "sequentialExecutionNs": sequential, "gaps": gaps});
    let serialized = intersect(&ready, &active);
    let serialization = json!({"status": state, "valueNs": length(&serialized), "upperBound": true,
        "parallelGateWaitNs": sum_ms(&timing.tool_calls, |call| call.parallel_gate_wait_ms),
        "gaps": gaps});
    (efficiency, effective, overlap, opportunity, serialization)
}

fn idle_gaps(ops: &Operations<'_>, timing: &TurnTiming, exact: bool) -> Value {
    let Some(timeline) = timing.activity_timeline.as_ref().filter(|_| exact) else {
        return unavailable("activity_timeline_not_exact");
    };
    let orchestration = merged(
        timeline
            .intervals
            .iter()
            .filter(|interval| interval.phase == "orchestration")
            .map(|interval| (interval.start_ns, interval.end_ns))
            .collect(),
    );
    // A running handler can leave the turn in orchestration; it still progresses work.
    let idle = subtract(
        &intersect(&orchestration, &merged(ops.ready_waits())),
        &merged(ops.leaf_executions()),
    );
    json!({
        "status": status(&ops.gaps),
        "valueNs": length(&idle),
        "gapCount": idle.len(),
        "maxGapNs": idle.iter().map(|(start, end)| end - start).max().unwrap_or(0),
        "orchestrationNs": length(&orchestration),
        "gaps": reasons(&ops.gaps),
    })
}

fn critical_share(path: &Result<PathAnalysis<'_>, Value>, duration: u64) -> (Value, Value, Value) {
    let analysis = match path {
        Ok(analysis) => analysis,
        Err(reason) => return (reason.clone(), reason.clone(), reason.clone()),
    };
    let utilization = json!({"status": "available",
        "ratio": ratio(analysis.execution(), analysis.elapsed),
        "numeratorNs": analysis.execution(), "denominatorNs": analysis.elapsed});
    let work_span = json!({"status": "available",
        "ratio": ratio(analysis.execution_work, analysis.execution_span),
        "workNs": analysis.execution_work, "spanNs": analysis.execution_span});
    let amplification = json!({"status": "available",
        "factor": ratio(duration, analysis.execution()),
        "turnDurationNs": duration, "baselineNs": analysis.execution(),
        "baseline": "execution_time_on_elapsed_critical_path",
        "crossRunBaseline": {"status": "requires_matched_comparison", "comparison": COMPARISON}});
    (utilization, work_span, amplification)
}

fn initialization_burden(ops: &Operations<'_>, timing: &TurnTiming, duration: u64) -> Value {
    let launches = ops
        .tools
        .iter()
        .filter_map(|operation| {
            span(ns(operation.call.handler_entry_at_ms), ns(operation.call.process_spawned_at_ms))
                .map(|(start, end)| (start.min(duration), end.min(duration)))
        })
        .collect::<Vec<_>>();
    let launch_union = length(&merged(launches.clone()));
    let local = &timing.local;
    let components = [
        local.startup_prewarm_wait_union_ns,
        local.executor_readiness_wait_union_ns,
        local.router_build_union_ns,
        launch_union,
    ];
    json!({
        "status": status(&ops.gaps),
        "boundsNs": {
            "lower": components.iter().copied().max().unwrap_or(0),
            "upper": components.iter().copied().fold(0u64, u64::saturating_add),
        },
        "componentsNs": {
            "startupPrewarmWait": local.startup_prewarm_wait_union_ns,
            "executorReadinessWait": local.executor_readiness_wait_union_ns,
            "routerBuild": local.router_build_union_ns,
            "toolProcessLaunchUnion": launch_union,
        },
        "toolProcessLaunch": summary(&launches),
        "gaps": reasons(&ops.gaps),
    })
}

fn material_progress(timing: &TurnTiming, duration: u64) -> (Value, Value, Value) {
    let mut events = Vec::new();
    if let Some(at) = ns(timing.milestones.first_successful_domain_action_ms) {
        events.push((at, "successful_domain_tool"));
    }
    for call in &timing.tool_calls {
        if tool_action_class(&call.tool_name) == ToolActionClass::Domain
            && call.outcome.as_deref() == Some("success")
            && let Some(at) = ns(call
                .handler_exit_at_ms
                .or(call.output_collected_at_ms)
                .or(call.delivered_at_ms))
        {
            events.push((at, "successful_domain_tool"));
        }
    }
    if let Some(tail) = timing
        .final_answer
        .as_ref()
        .and_then(|answer| answer.tail.as_ref())
        .filter(|tail| tail.answer_origin.is_some())
    {
        if let Some(at) = ns(tail.first_content_host_ms) {
            events.push((at, "accepted_final_first_content"));
        }
        if let Some(at) = ns(tail.full_content_host_ms) {
            events.push((at, "accepted_final_full_content"));
        }
    }
    let (Some(first), Some(last)) = (
        events.iter().min_by_key(|event| event.0),
        events.iter().max_by_key(|event| event.0),
    ) else {
        let missing = unavailable("no_material_progress_observed");
        return (missing.clone(), missing.clone(), missing);
    };
    (
        json!({"status": "available", "valueNs": first.0, "event": first.1}),
        json!({"status": "available", "valueNs": last.0, "event": last.1,
            "shareOfTurn": ratio(last.0.min(duration), duration)}),
        json!({"status": "available", "valueNs": duration.saturating_sub(last.0),
            "finalizationNs": timing.exclusive.finalization_ns}),
    )
}

fn regression_components(timing: &TurnTiming, path: &Result<PathAnalysis<'_>, Value>) -> Value {
    let exclusive = &timing.exclusive;
    let parts = [
        ("modelOnlyNs", exclusive.model_only_ns),
        ("toolOnlyNs", exclusive.tool_only_ns),
        ("modelPlusToolNs", exclusive.model_plus_tool_ns),
        ("interactiveOnlyWaitNs", exclusive.interactive_only_wait_ns),
        ("interactivePlusMachineNs", exclusive.interactive_plus_machine_ns),
        ("retryOnlyNs", exclusive.retry_only_ns),
        ("orchestrationNs", exclusive.orchestration_ns),
        ("standaloneWorkNs", exclusive.standalone_work_ns),
        ("finalizationNs", exclusive.finalization_ns),
        ("unclassifiedNs", exclusive.unclassified_ns),
    ];
    let sum = parts.iter().fold(0u64, |sum, (_, value)| sum.saturating_add(*value));
    let critical = path.as_ref().ok().map(|analysis| json!({
        "modelNs": analysis.model, "toolNs": analysis.tool, "nestedCellNs": analysis.nested_cell,
        "waitNs": analysis.wait, "handoffNs": analysis.handoff, "retryHandoffNs": analysis.retry_handoff,
    }));
    json!({
        "status": "requires_matched_comparison",
        "components": {
            "exclusive": parts.iter().map(|(name, value)| (name.to_string(), json!(value))).collect::<serde_json::Map<_, _>>(),
            "exclusiveSumNs": sum,
            "additive": timing.profile_valid && sum == timing.inclusive_duration_ns,
            "criticalPath": critical,
        },
        "comparison": COMPARISON,
    })
}

fn recovery_amplification(ops: &Operations<'_>, timing: &TurnTiming, path: &Result<PathAnalysis<'_>, Value>, duration: u64) -> Value {
    let seconds = |value: &Value| value.as_f64().filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| (value * 1e9).round() as u64);
    let mut gaps = ops.gaps.clone();
    let mut failures = Vec::new();
    let mut recovery = Vec::new();
    let mut requests = BTreeSet::new();
    let telemetry = timing.recovery_telemetry.as_ref();
    match telemetry {
        Some(telemetry) => {
            if telemetry["scope_complete"] != true || telemetry["events_complete"] != true {
                gaps.insert("recovery_telemetry_incomplete");
            }
            for attempt in telemetry["attempts"].as_array().into_iter().flatten() {
                // A cancelled attempt (for example a user interrupt) is not a
                // failure; a retry it triggered is still recovery-induced.
                let failed = attempt["outcome"] == "failed";
                if !failed && attempt["recovery_induced"] != true {
                    continue;
                }
                if let Some(id) = attempt["request_id"].as_str() {
                    requests.insert(id.to_owned());
                }
                if let Some(attempt_span) = span(seconds(&attempt["start_time"]), seconds(&attempt["end_time"])) {
                    if failed {
                        failures.push(attempt_span);
                    } else {
                        recovery.push(attempt_span);
                    }
                }
            }
            for event in telemetry["events"].as_array().into_iter().flatten() {
                if let Some(event_span) = span(seconds(&event["start_time"]), seconds(&event["end_time"])) {
                    recovery.push(event_span);
                }
            }
        }
        None => {
            gaps.insert("recovery_telemetry_not_recorded");
        }
    }
    let mut owners = BTreeSet::new();
    for operation in &ops.tools {
        if matches!(operation.call.outcome.as_deref(), Some("failed" | "failure" | "error")) {
            owners.insert(operation.call.execution_id.0.clone());
            failures.extend(operation.envelope);
        }
    }
    for (request, envelope) in &ops.models {
        if matches!(request.generation_purpose,
            Some(TurnTimingGenerationPurpose::FailureDiagnosis | TurnTimingGenerationPurpose::Repair))
        {
            owners.insert(format!("generation-{}", request.generation_index));
            recovery.push(*envelope);
        }
    }
    let clip = |spans: Vec<Span>| merged(spans.into_iter().map(|(start, end)| (start.min(duration), end.min(duration))).collect());
    let failures = clip(failures);
    let affected = clip(failures.iter().copied().chain(recovery).collect());
    // Owner lanes ("tool-execution-N", "generation-N") contain no ':', so a
    // node belongs to an owner exactly when its id's first segment matches.
    let critical = path.as_ref().ok().map(|analysis| {
        analysis.path.iter().filter(|node| {
            node.id.contains(":retry-handoff-")
                || node.sampling_request_id.as_ref().is_some_and(|id| requests.contains(id))
                || owners.contains(node.id.split(':').next().unwrap_or_default())
        }).filter_map(|node| node_duration(node)).fold(0u64, u64::saturating_add)
    });
    json!({
        "status": status(&gaps),
        "valueNs": length(&affected),
        "failureNs": length(&failures),
        "amplificationFactor": ratio(length(&affected), length(&failures)),
        "criticalPathNs": critical,
        "gaps": reasons(&gaps),
    })
}

fn delegation(ops: &Operations<'_>, timing: &TurnTiming) -> Value {
    let Some(agents) = timing
        .measurement_evidence
        .as_ref()
        .map(|evidence| &evidence["causalAgents"])
        .filter(|agents| agents.is_object())
    else {
        return json!({"status": "available", "children": 0, "joined": 0, "rows": []});
    };
    let envelope = |execution: &str| ops.tools.iter()
        .find(|operation| operation.call.execution_id.0 == execution)
        .and_then(|operation| operation.envelope);
    let mut spawns: BTreeMap<&str, (&str, u64)> = BTreeMap::new();
    let mut joins: BTreeMap<&str, Vec<(&str, u64)>> = BTreeMap::new();
    for row in agents["rows"].as_array().into_iter().flatten() {
        let (Some(child), Some(execution), Some(at)) = (
            row["childSession"].as_str(),
            row["executionId"].as_str(),
            ns(row["atMs"].as_u64()),
        ) else {
            continue;
        };
        match row["kind"].as_str() {
            Some("spawn") => {
                spawns.entry(child).or_insert((execution, at));
            }
            Some("join") => joins.entry(child).or_default().push((execution, at)),
            _ => {}
        }
    }
    let mut totals = [0u64; 4];
    let mut joined = 0usize;
    let mut rows = Vec::new();
    for (child, (spawn_execution, spawn_at)) in &spawns {
        let join = joins.get(child).and_then(|rows| rows.iter()
            .filter(|(_, at)| at >= spawn_at).min_by_key(|(_, at)| *at));
        let overhead = envelope(spawn_execution).map(|(start, end)| end - start);
        let Some((join_execution, join_at)) = join else {
            rows.push(json!({"childSession": child, "spawnAtNs": spawn_at, "joined": false,
                "spawnOverheadNs": overhead}));
            continue;
        };
        joined += 1;
        let delegated = join_at - spawn_at;
        let blocked = envelope(join_execution)
            .map(|(start, _)| length(&intersect(&[(start, *join_at)], &[(*spawn_at, *join_at)])))
            .unwrap_or(0);
        let concurrent = delegated.saturating_sub(blocked);
        let break_even = overhead.unwrap_or(0).saturating_add(blocked);
        for (total, value) in totals.iter_mut().zip([overhead.unwrap_or(0), blocked, concurrent, break_even]) {
            *total = total.saturating_add(value);
        }
        rows.push(json!({"childSession": child, "spawnAtNs": spawn_at, "joinAtNs": join_at,
            "joined": true, "spawnOverheadNs": overhead, "delegatedSpanNs": delegated,
            "parentBlockedNs": blocked, "parentConcurrentNs": concurrent,
            "breakEvenLatencyNs": break_even}));
    }
    let omitted = rows.len().saturating_sub(MAX_DELEGATION_ROWS);
    rows.truncate(MAX_DELEGATION_ROWS);
    json!({
        "status": if agents["complete"] == true { "available" } else { "partial" },
        "children": spawns.len(),
        "joined": joined,
        "totalsNs": {"spawnOverhead": totals[0], "parentBlocked": totals[1],
            "parentConcurrent": totals[2], "breakEvenLatency": totals[3]},
        "rows": rows,
        "omittedRows": omitted,
        "childWorkComparison": {"status": "requires_child_capture",
            "comparison": "kdr tools diagnose wall-clock-profile --rollout ROOT --descendant-rollout CHILD"},
    })
}

fn request_inventory_complete(timing: &TurnTiming) -> bool {
    let generations = timing
        .model_requests
        .iter()
        .map(|request| request.generation_index)
        .collect::<BTreeSet<_>>();
    timing.model_request_history_truncated == Some(false)
        && generations
            .into_iter()
            .eq(0..timing.counters.logical_generation_count)
        && timing.counters.saturation_count == 0
}

fn tool_inventory_complete(timing: &TurnTiming) -> bool {
    timing.tool_closure.measurement_available
        && timing.tool_closure.complete
        && timing.tool_call_timing_overflow == 0
        && timing.tool_calls.len() == timing.tool_closure.accepted_count as usize
}

/// Builds `performanceObservations.wallClockProfile` for a terminal record.
pub(super) fn snapshot(timing: &TurnTiming) -> Value {
    let duration = timing.inclusive_duration_ns;
    let timeline = timing.activity_timeline.as_ref();
    let exact = timeline.is_some_and(|timeline| {
        timeline.version == 1
            && !timeline.tail_unknown
            && timeline.coverage_complete
            && timeline.dropped_intervals == 0
            && timeline.duration_ns == duration
    });
    let mut ops = Operations::collect(timing, duration);
    if !request_inventory_complete(timing) {
        ops.gaps.insert("incomplete_request_inventory");
    }
    if !tool_inventory_complete(timing) {
        ops.gaps.insert("incomplete_tool_inventory");
    }
    let parents = ops
        .tools
        .iter()
        .filter(|operation| operation.has_children)
        .map(|operation| operation.call.execution_id.0.as_str())
        .collect::<BTreeSet<_>>();
    let path = analyze_graph(timing.dependency_graph.as_ref(), &parents);
    let (utilization, work_span, amplification) = critical_share(&path, duration);
    let (efficiency, effective, overlap, opportunity, serialization) =
        concurrency_metrics(&ops, timing, duration);
    let (first, last, post) = material_progress(timing, duration);
    let valid = timing.profile_valid;
    let unions = &timing.unions;
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "basis": "derived_from_this_terminal_turn_timing",
        "contract": "codex-rs/turn-timing/WALL_CLOCK_PROFILE.md",
        "clockDomainId": timeline.and_then(|timeline| timeline.clock_domain_id.clone()),
        "turnDurationNs": duration,
        "coverage": {
            "profileValid": valid,
            "activityTimelineExact": exact,
            "dependencyGraphComplete": path.is_ok(),
            "requestInventoryComplete": request_inventory_complete(timing),
            "toolInventoryComplete": tool_inventory_complete(timing),
            "blockingEvidenceRecorded": timeline.is_some_and(|timeline| timeline.blocking.is_some()),
            "operationGaps": reasons(&ops.gaps),
            "resolution": "operation endpoints are millisecond offsets; timeline and graph spans are nanoseconds",
        },
        "metrics": {
            "criticalPathDuration": critical_path_duration(&path),
            "exclusiveExecutionLatency": execution_latency(&ops, duration, true),
            "inclusiveExecutionLatency": execution_latency(&ops, duration, false),
            "inferenceWaitRatio": share(unions.model_active_union_ns, timing.exclusive.model_only_ns, duration, valid),
            "toolWaitRatio": share(unions.tool_active_union_ns, timing.exclusive.tool_only_ns, duration, valid),
            "schedulerInducedLatency": scheduler_latency(&ops, timing),
            "synchronizationWaitTime": synchronization_wait(timing, duration),
            "parallelExecutionEfficiency": efficiency,
            "effectiveConcurrency": effective,
            "executionOverlapRatio": overlap,
            "idleGapDuration": idle_gaps(&ops, timing, exact),
            "criticalPathUtilization": utilization,
            "workSpanRatio": work_span,
            "queueingDelayAttribution": queueing_attribution(&ops, timing, duration),
            "latencyAmplificationFactor": amplification,
            "processInitializationBurden": initialization_burden(&ops, timing, duration),
            "timeToFirstMaterialProgress": first,
            "timeToLastMaterialProgress": last,
            "postCompletionOverhead": post,
            "wallClockRegressionAttribution": regression_components(timing, &path),
            "parallelismOpportunityRatio": opportunity,
            "avoidableSerializationTime": serialization,
            "recoveryLatencyAmplification": recovery_amplification(&ops, timing, &path, duration),
            "delegationBreakEvenLatency": delegation(&ops, timing),
            "endToEndSpeedupEfficiency": {
                "status": "requires_matched_comparison",
                "comparison": COMPARISON,
                "inputs": ["turnDurationNs", "metrics.wallClockRegressionAttribution.components",
                    "metrics.inclusiveExecutionLatency.classes"],
            },
        },
    })
}
