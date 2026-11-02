//! Reuse owner-recorded relations; chronology alone never creates an edge.
//! This projection adds no tasks, locks, timers or scheduler changes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use codex_protocol::protocol::{TurnTiming, TurnTimingToolCallSource};
use codex_protocol::timing::{
    TurnTimingDependencyGraph, TurnTimingDependencyNode as Node,
    TurnTimingDependencyNodeKind as Kind,
};

fn node(id: String, kind: Kind, start_ns: Option<u64>, end_ns: Option<u64>) -> Node {
    Node {
        id,
        kind,
        start_ns,
        end_ns,
        depends_on: Vec::new(),
        sampling_request_id: None,
        repeat_of: None,
        work_identity: None,
        input_identity: None,
        necessity: None,
        necessity_evidence: Vec::new(),
        evidence_ref: None,
    }
}

fn ns(ms: Option<u64>) -> Option<u64> {
    ms.and_then(|ms| ms.checked_mul(1_000_000))
}

pub(super) fn snapshot(timing: &TurnTiming) -> Option<TurnTimingDependencyGraph> {
    let timeline = timing.activity_timeline.as_ref()?;
    if timeline.tail_unknown {
        return None;
    }
    let mut reasons = BTreeSet::new();
    if !timing.profile_valid
        || !timeline.coverage_complete
        || timeline.version != 1
        || timeline.duration_ns != timing.inclusive_duration_ns
        || timeline.clock_domain_id.is_none()
        || timeline.dropped_intervals != 0
        || timeline.open_process_count != 0
    {
        reasons.insert("incomplete_clock_or_activity_coverage");
    }
    if timing.model_request_history_truncated != Some(false)
        || timing.model_requests.len() != timing.counters.model_request_count as usize
        || timing.counters.saturation_count != 0
    {
        reasons.insert("incomplete_request_inventory");
    }
    if !timing.tool_closure.measurement_available
        || !timing.tool_closure.complete
        || timing.tool_call_timing_overflow != 0
        || timing.tool_calls.len() != timing.tool_closure.accepted_count as usize
    {
        reasons.insert("incomplete_tool_inventory");
    }
    let mut nodes = BTreeMap::new();
    nodes.insert(
        "start".to_owned(),
        node("start".to_owned(), Kind::Boundary, Some(0), Some(0)),
    );
    for (index, request) in timing.model_requests.iter().enumerate() {
        let id = format!("generation-{}", request.generation_index);
        let mut row = node(
            id.clone(),
            Kind::Model,
            ns(request.dispatch_ms),
            ns(request.completed_ms),
        );
        row.sampling_request_id = request.sampling_request_id.clone();
        if row.sampling_request_id.is_none() {
            reasons.insert("missing_request_identity");
        }
        // Only the initial request is rooted by core-turn acceptance. Subsequent
        // requests require actual consumes_tool_result relations from dispatch.
        if index == 0 {
            row.depends_on.push("start".to_owned());
        }
        if nodes.insert(id, row).is_some() {
            reasons.insert("ambiguous_generation_identity");
        }
    }
    for call in &timing.tool_calls {
        if call.source != TurnTimingToolCallSource::Direct {
            reasons.insert("nested_fork_join_not_recorded");
        }
        let id = call.execution_id.0.clone();
        let row = node(
            id.clone(),
            Kind::Tool,
            ns(call.accepted_at_ms),
            ns(call.delivered_at_ms),
        );
        if id.is_empty() || nodes.insert(id, row).is_some() {
            reasons.insert("ambiguous_tool_identity");
        }
    }
    if let Some(blocking) = &timeline.blocking {
        if blocking.dropped_dependencies != 0 || blocking.dropped_intervals != 0 {
            reasons.insert("dropped_dependency_evidence");
        }
        for edge in &blocking.dependencies {
            // Emission depends on a model's produced prefix, not completion of
            // the entire request. Millisecond rounding can hide that overlap;
            // even equal/ordered endpoints cannot certify the full-node edge.
            if edge.relation == "emitted_by" {
                reasons.insert("model_prefix_release_not_full_request_dependency");
            }
            // Await observations internal to an envelope are retained on the
            // timeline. They do not imply a task-completion prerequisite.
            if !matches!(
                edge.relation.as_str(),
                "emitted_by" | "consumes_tool_result"
            ) {
                continue;
            }
            if !nodes.contains_key(&edge.prerequisite) {
                reasons.insert("missing_prerequisite_identity");
            }
            if let Some(row) = nodes.get_mut(&edge.dependent) {
                if !row.depends_on.contains(&edge.prerequisite) {
                    row.depends_on.push(edge.prerequisite.clone());
                }
            } else {
                reasons.insert("missing_dependent_identity");
            }
        }
    } else {
        reasons.insert("causal_relations_not_recorded");
    }

    // A host handoff measures the whole gap between explicit prerequisite
    // completion and dependent start. Its cause (execution/scheduling/waiting)
    // is unspecified; it is not CPU time or an inferred resource wait.
    let work_ids = nodes
        .keys()
        .filter(|id| id.as_str() != "start")
        .cloned()
        .collect::<Vec<_>>();
    for (index, id) in work_ids.iter().enumerate() {
        let row = &nodes[id];
        let (Some(start), Some(end)) = (row.start_ns, row.end_ns) else {
            reasons.insert("missing_node_endpoint");
            continue;
        };
        if start > end || end > timeline.duration_ns || row.depends_on.is_empty() {
            reasons.insert("unbound_or_invalid_task_span");
            continue;
        }
        let ends = row
            .depends_on
            .iter()
            .map(|dep| nodes.get(dep).and_then(|node| node.end_ns))
            .collect::<Option<Vec<_>>>();
        let Some(ends) = ends else {
            reasons.insert("missing_prerequisite_endpoint");
            continue;
        };
        let previous_end = ends.into_iter().max().unwrap_or(0);
        if previous_end > start {
            // Full-request v1 nodes cannot describe an emitted prefix fork.
            // Preserve the facts instead of moving endpoints to fit the DAG.
            reasons.insert("streaming_or_nested_prefix_fork_requires_segmented_graph");
            continue;
        }
        if previous_end < start {
            let handoff_id = format!("host-handoff-{index}");
            let mut handoff = node(
                handoff_id.clone(),
                Kind::Boundary,
                Some(previous_end),
                Some(start),
            );
            handoff.depends_on = row.depends_on.clone();
            nodes.get_mut(id)?.depends_on = vec![handoff_id.clone()];
            nodes.insert(handoff_id, handoff);
        }
    }
    // The accepted-call closure owner requires every native execution to settle
    // before core terminal publication. Incomplete closure keeps complete=false.
    let last_end = work_ids
        .iter()
        .filter_map(|id| nodes[id].end_ns)
        .max()
        .unwrap_or(0);
    let mut tail = node(
        "host-terminal".to_owned(),
        Kind::Boundary,
        Some(last_end),
        Some(timeline.duration_ns),
    );
    tail.depends_on = if work_ids.is_empty() {
        vec!["start".to_owned()]
    } else {
        work_ids
    };
    nodes.insert(tail.id.clone(), tail);
    let mut finish = node(
        "finish".to_owned(),
        Kind::Boundary,
        Some(timeline.duration_ns),
        Some(timeline.duration_ns),
    );
    finish.depends_on.push("host-terminal".to_owned());
    nodes.insert(finish.id.clone(), finish);

    // Bounds follow the existing 1,024 request / 1,024 call / 8,192 edge
    // retention limits; at most one handoff is added per native work node.
    let mut pending = nodes
        .iter()
        .map(|(id, row)| (id.clone(), row.depends_on.len()))
        .collect::<BTreeMap<_, _>>();
    let mut successors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in nodes.values() {
        for dep in &row.depends_on {
            successors
                .entry(dep.clone())
                .or_default()
                .push(row.id.clone());
        }
    }
    let mut ready = VecDeque::from(["start".to_owned()]);
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for child in successors.get(&id).into_iter().flatten() {
            if let Some(count) = pending.get_mut(child) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.push_back(child.clone());
                }
            }
        }
    }
    if visited != nodes.len() {
        reasons.insert("cyclic_or_disconnected_task_graph");
    }
    // Validated by the existing annotation owner before any plan effects.
    // Labels do not turn an incomplete graph into a complete one.
    if let Some(rows) = timing
        .measurement_evidence
        .as_ref()
        .and_then(|evidence| evidence["dependencyNodes"].as_array())
    {
        for annotation in rows {
            if let Some(row) = annotation["id"].as_str().and_then(|id| nodes.get_mut(id)) {
                row.repeat_of = annotation["repeatOf"].as_str().map(str::to_owned);
                row.work_identity = annotation["workIdentity"].as_str().map(str::to_owned);
                row.input_identity = annotation["inputIdentity"].as_str().map(str::to_owned);
                row.necessity = annotation["necessity"].as_str().map(str::to_owned);
                row.evidence_ref = annotation["evidenceRef"].as_str().map(str::to_owned);
                row.necessity_evidence = annotation["necessityEvidence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|reference| reference.as_str().map(str::to_owned))
                    .collect();
            }
        }
    }
    Some(TurnTimingDependencyGraph {
        schema_version: 1,
        complete: reasons.is_empty(),
        includes_descendants: false,
        clock_domain_id: timeline.clock_domain_id.clone(),
        started_at_ns: timeline.started_at_ns,
        duration_ns: timeline.duration_ns,
        start_node: "start".to_owned(),
        finish_node: "finish".to_owned(),
        nodes: nodes.into_values().collect(),
        unavailable_reasons: reasons.into_iter().map(str::to_owned).collect(),
    })
}
