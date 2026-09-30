//! Causal investigation state, owned by the existing session plan store.
//!
//! Reports are model-authored claims, not an automatic proof of causality. The
//! harness verifies their transitions and their connection to completed tools.
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(crate) use codex_protocol::plan_tool::investigation::*;
use serde_json::Value;

const SATURATION_GENERATIONS: u32 = 3;

#[derive(Clone, Debug)]
pub(crate) struct ObservationTarget {
    id: String,
    question: String,
    hypothesis_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct InvestigationState {
    pub(crate) current: Option<Investigation>,
    observations: BTreeMap<String, ObservationTarget>,
    consumed: BTreeSet<String>,
    causal_progress: bool,
    generations_without_narrowing: u32,
    checkpoint_required: bool,
    reproduced: bool,
    /// The user replied after the report became blocked; they may have
    /// supplied the missing evidence, so reads are no longer stopped.
    user_input_since_blocked: bool,
}

impl InvestigationState {
    pub(crate) fn validate(&self, next: &Investigation) -> Result<bool, String> {
        let nonblank = |text: &str| !text.trim().is_empty() && text.len() <= 8192;
        if !nonblank(&next.symptom)
            || next.hypotheses.is_empty()
            || next.hypotheses.len() > 16
            || next.unknowns.len() > 32
            || next.unknowns.iter().any(|text| !nonblank(text))
        {
            return Err(
                "investigation requires a bounded symptom, hypotheses, and named unknowns".into(),
            );
        }
        let ids: BTreeSet<_> = next.hypotheses.iter().map(|h| h.id.as_str()).collect();
        if ids.len() != next.hypotheses.len()
            || next
                .hypotheses
                .iter()
                .any(|h| !nonblank(&h.id) || !nonblank(&h.explanation))
        {
            return Err("hypotheses require unique stable IDs and explanations".into());
        }
        let observation = &next.next_observation;
        if !nonblank(&observation.id)
            || !nonblank(&observation.question)
            || observation.hypothesis_ids.is_empty()
            || observation
                .hypothesis_ids
                .iter()
                .any(|id| !ids.contains(id.as_str()))
            || observation.expected_outcomes.len() < 2
            || observation.expected_outcomes.len() > 16
            || observation
                .expected_outcomes
                .iter()
                .any(|outcome| !nonblank(outcome))
            || observation
                .expected_outcomes
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                < 2
        {
            return Err("name the hypothesis and at least two distinct outcomes the next observation would distinguish".into());
        }
        if !observation.obtainable && !observation.blocker.as_deref().is_some_and(nonblank) {
            return Err("unavailable discriminating evidence requires a concrete blocker".into());
        }
        if matches!(next.phase, Phase::Blocked) != !observation.obtainable
            && !matches!(next.phase, Phase::Resolved | Phase::Cancelled)
        {
            return Err("use blocked when the required observation is unavailable; do not continue optional investigation".into());
        }
        let Some(previous) = &self.current else {
            if next.phase != Phase::Investigating && next.phase != Phase::Blocked
                || next.finding.is_some()
                || next
                    .hypotheses
                    .iter()
                    .any(|h| h.status != HypothesisStatus::Open)
            {
                return Err(
                    "start with the symptom and open hypotheses before claiming a cause".into(),
                );
            }
            return Ok(false);
        };
        if matches!(previous.phase, Phase::Resolved | Phase::Cancelled)
            && previous.symptom != next.symptom
        {
            return Self::default().validate(next);
        }
        if previous.symptom != next.symptom
            || previous.hypotheses.iter().any(|old| {
                !next
                    .hypotheses
                    .iter()
                    .any(|new| new.id == old.id && new.explanation == old.explanation)
            })
        {
            return Err("preserve the active symptom and hypothesis identities; do not reset the investigation by renaming them".into());
        }
        let changed_hypotheses: Vec<_> = next
            .hypotheses
            .iter()
            .filter(|new| {
                previous
                    .hypotheses
                    .iter()
                    .find(|old| old.id == new.id)
                    .is_some_and(|old| old.status != new.status)
            })
            .collect();
        if next.hypotheses.iter().any(|new| {
            !previous.hypotheses.iter().any(|old| old.id == new.id)
                && new.status != HypothesisStatus::Open
        }) {
            return Err(
                "new hypotheses must start open before evidence can establish or rule them out"
                    .into(),
            );
        }
        if previous.next_observation.id == next.next_observation.id
            && (previous.next_observation.question != next.next_observation.question
                || previous.next_observation.hypothesis_ids != next.next_observation.hypothesis_ids)
        {
            return Err(
                "use a new observation ID when changing its question or targeted hypotheses".into(),
            );
        }
        let removed_unknowns: Vec<_> = previous
            .unknowns
            .iter()
            .filter(|old| !next.unknowns.contains(old))
            .collect();
        let finding_changed = next.finding != previous.finding;
        let mut progress = false;
        if finding_changed && let Some(finding) = &next.finding {
            let observation = self.observations.get(&finding.observation_id)
                .ok_or("finding must cite a completed tool observation from this investigation")?;
            if self.consumed.contains(&finding.observation_id)
                || finding.hypothesis_ids.is_empty()
                || finding
                    .hypothesis_ids
                    .iter()
                    .any(|id| !observation.hypothesis_ids.contains(id))
                || !nonblank(&finding.uncertainty)
                || !nonblank(&finding.evidence)
                || !nonblank(&finding.conclusion)
            {
                return Err("finding requires unused evidence, its targeted hypothesis, uncertainty, and conclusion".into());
            }
            progress = match finding.kind {
                FindingKind::UncertaintyResolved => {
                    removed_unknowns.contains(&&finding.uncertainty)
                }
                FindingKind::HypothesisEliminated => changed_hypotheses.iter().any(|h| {
                    h.status == HypothesisStatus::RuledOut && finding.hypothesis_ids.contains(&h.id)
                }),
                FindingKind::DecisionChanged => {
                    observation.question != next.next_observation.question
                }
                FindingKind::FailureReproduced => !self.reproduced,
                FindingKind::CauseEstablished => changed_hypotheses.iter().any(|h| {
                    h.status == HypothesisStatus::Established
                        && finding.hypothesis_ids.contains(&h.id)
                }),
            };
            if !progress {
                return Err("finding did not resolve a named uncertainty, eliminate a hypothesis, change the next decision, reproduce the failure, or establish a cause".into());
            }
            if changed_hypotheses
                .iter()
                .any(|h| !finding.hypothesis_ids.contains(&h.id))
            {
                return Err(
                    "the finding must address every hypothesis whose status changes".into(),
                );
            }
        }
        if (!changed_hypotheses.is_empty() || !removed_unknowns.is_empty()) && !progress {
            return Err(
                "ruling out hypotheses or removing unknowns requires a discriminating finding"
                    .into(),
            );
        }
        if matches!(next.phase, Phase::Implementing | Phase::Resolved)
            && (!next
                .hypotheses
                .iter()
                .any(|h| h.status == HypothesisStatus::Established)
                || next
                    .hypotheses
                    .iter()
                    .any(|h| h.status == HypothesisStatus::Open))
        {
            return Err(
                "causal-evidence gate: establish the cause before implementation or resolution"
                    .into(),
            );
        }
        if next
            .hypotheses
            .iter()
            .any(|h| h.status == HypothesisStatus::Established)
            && !previous
                .hypotheses
                .iter()
                .any(|h| h.status == HypothesisStatus::Established)
            && !next
                .finding
                .as_ref()
                .is_some_and(|f| f.kind == FindingKind::CauseEstablished)
        {
            return Err("an established hypothesis requires a cause_established finding".into());
        }
        Ok(progress)
    }

    pub(crate) fn commit(&mut self, next: Investigation, progress: bool) {
        if self
            .current
            .as_ref()
            .is_some_and(|old| old.symptom != next.symptom)
        {
            *self = Self::default();
        }
        if progress && let Some(finding) = &next.finding {
            self.consumed.insert(finding.observation_id.clone());
            self.reproduced |= finding.kind == FindingKind::FailureReproduced;
        }
        self.causal_progress |= progress;
        self.checkpoint_required = false;
        self.user_input_since_blocked = false;
        self.current = Some(next);
    }

    /// Saturation pressure and the blocked-evidence stop are per user turn:
    /// new input may supply the evidence or redirect the task. The recorded
    /// report and its causal gate for implementation remain in force.
    pub(crate) fn accepted_user_input(&mut self) {
        self.generations_without_narrowing = 0;
        self.checkpoint_required = false;
        self.user_input_since_blocked = true;
    }

    pub(crate) fn restore(&mut self, current: Option<Investigation>) {
        // A resumed report retains its gates, but old tool receipts cannot be
        // used to manufacture another transition in the new execution.
        let mut current = current;
        if let Some(report) = &mut current {
            // An exact experiment permit is never replay-safe across resume.
            report.next_observation.diagnostic_action = None;
        }
        *self = Self {
            current,
            ..Self::default()
        };
    }

    pub(crate) fn target(&self) -> Option<ObservationTarget> {
        self.current
            .as_ref()
            .filter(|i| i.phase == Phase::Investigating)
            .map(|i| ObservationTarget {
                id: i.next_observation.id.clone(),
                question: i.next_observation.question.clone(),
                hypothesis_ids: i.next_observation.hypothesis_ids.clone(),
            })
    }

    pub(crate) fn record_observation(&mut self, target: ObservationTarget) {
        if self.observations.len() >= 256 {
            self.observations
                .retain(|id, _| !self.consumed.contains(id));
        }
        if self.observations.len() < 256 {
            self.observations.entry(target.id.clone()).or_insert(target);
        }
    }

    /// None leaves ordinary implementation/inventory progress unchanged.
    pub(crate) fn finish_generation(&mut self) -> Option<(bool, Option<String>)> {
        let current = self.current.as_ref()?;
        let narrowed = std::mem::take(&mut self.causal_progress);
        if narrowed {
            self.generations_without_narrowing = 0;
            return Some((true, None));
        }
        if matches!(
            current.phase,
            Phase::Implementing | Phase::Resolved | Phase::Cancelled
        ) {
            return None;
        }
        self.generations_without_narrowing = self.generations_without_narrowing.saturating_add(1);
        if self.generations_without_narrowing % SATURATION_GENERATIONS != 0 {
            return Some((false, None));
        }
        self.checkpoint_required = true;
        Some((
            false,
            Some(format!(
                "Investigation saturation checkpoint: {} generations without a recorded causal narrowing. \
             New files, facts, and changed tool arguments are not diagnostic progress. Before further \
             investigation, use update_plan.investigation to report current hypotheses and their \
             ruled-out statuses, remaining unknowns, the single discriminating observation, its \
             expected outcomes, and whether it is obtainable. If unavailable, mark blocked and \
             report the missing evidence instead of continuing source archaeology. Do not claim \
             a cause or begin implementation without a supported finding. Current state: {}",
                self.generations_without_narrowing,
                serde_json::to_string(current).unwrap_or_default(),
            )),
        ))
    }

    pub(crate) fn admit(
        &mut self,
        tool: &str,
        input: &Value,
        may_mutate: bool,
        control: bool,
    ) -> Result<(), String> {
        let Some(current) = self.current.as_mut() else {
            return Ok(());
        };
        if control || matches!(current.phase, Phase::Resolved | Phase::Cancelled) {
            return Ok(());
        }
        if self.checkpoint_required {
            return Err("investigation saturation: submit the structured investigation checkpoint before more tools".into());
        }
        if current.phase == Phase::Blocked && !self.user_input_since_blocked {
            return Err("required discriminating evidence is unavailable; report the blocker, or update its availability with new evidence".into());
        }
        if !may_mutate || current.phase == Phase::Implementing {
            return Ok(());
        }
        if current
            .next_observation
            .diagnostic_action
            .as_ref()
            .is_some_and(|action| action.tool == tool && action.input == *input)
        {
            // One exact experiment, not a general write authorization. Normal
            // permission, cancellation and sandbox checks still apply.
            current.next_observation.diagnostic_action = None;
            return Ok(());
        }
        Err("causal-evidence gate: implementation requires an established cause. For a necessary diagnostic experiment, declare this exact tool/input as next_observation.diagnostic_action and the outcomes it distinguishes; this does not authorize production repair or bypass permissions".into())
    }
}

/// Runs at the final core handler boundary, after hook rewrites, for direct and
/// code-mode calls alike. It supplements rather than replaces authorization.
pub(crate) fn admit_tool(
    invocation: &crate::tools::context::ToolInvocation,
) -> Result<Option<ObservationTarget>, crate::FunctionCallError> {
    use crate::agent::task_capabilities::TypedToolClass;
    use crate::tools::context::ToolPayload;
    if invocation.session.services.plan_store.investigation.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .current.as_ref().is_none_or(|report| matches!(report.phase, Phase::Resolved | Phase::Cancelled))
    {
        return Ok(None);
    }
    let name = invocation.tool_name.to_string();
    let plain = invocation.tool_name.namespace.is_none();
    let control = plain
        && matches!(
            name.as_str(),
            "update_plan"
                | "request_user_input"
                | "tool_search"
                | "context_checkpoint"
                | "new_context"
        );
    let class = invocation
        .step_context
        .tool_router()
        .map(|router| {
            router.classify_tool_name(&invocation.step_context.turn, &invocation.tool_name)
        })
        .unwrap_or(TypedToolClass::Unknown);
    let control = control
        || class == TypedToolClass::CodeModeControl
        || class == TypedToolClass::AgentCommunication;
    let input = match &invocation.payload {
        ToolPayload::Function { arguments } => {
            serde_json::from_str(arguments).unwrap_or(Value::Null)
        }
        ToolPayload::Custom { input } => Value::String(input.clone()),
        _ => Value::Null,
    };
    let safe_shell = if plain && name == "write_stdin" {
        input["chars"]
            .as_str()
            .is_none_or(|chars| chars.is_empty() || chars == "\u{3}")
    } else if class == TypedToolClass::Shell
        || plain && matches!(name.as_str(), "exec_command" | "shell_command" | "shell")
    {
        let argv = if let Some(program) = input["program"].as_str() {
            input["args"]
                .as_array()
                .map(|args| {
                    std::iter::once(Some(program.to_string()))
                        .chain(args.iter().map(|arg| arg.as_str().map(str::to_string)))
                        .collect::<Option<Vec<_>>>()
                })
                .unwrap_or_else(|| Some(vec![program.to_string()]))
        } else if input["shell"].is_null() {
            input["cmd"]
                .as_str()
                .or_else(|| input["command"].as_str())
                .or_else(|| input["script_body"].as_str())
                .and_then(|script| {
                    invocation
                        .session
                        .services
                        .user_shell
                        .derive_exec_args(script, false)
                        .ok()
                })
        } else {
            None
        };
        argv.is_some_and(|argv| {
            codex_shell_command::is_safe_command::is_known_safe_command(&argv)
        })
    } else {
        false
    };
    let read = class == TypedToolClass::ReadSearch
        || plain
            && matches!(
                name.as_str(),
                "read_file" | "read_tool_output" | "list_files" | "view_image"
            );
    let mut state = invocation
        .session
        .services
        .plan_store
        .investigation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Polling and cancellation must remain available at a checkpoint, but a
    // completed process observation can still supply the declared evidence.
    let process_monitor = plain && name == "write_stdin" && safe_shell;
    state
        .admit(
            &name,
            &input,
            !read && !safe_shell,
            control || process_monitor,
        )
        .map_err(crate::FunctionCallError::RespondToModel)?;
    Ok((!control).then(|| state.target()).flatten())
}

pub(crate) fn schema() -> Value {
    use serde_json::json;
    let text = json!({"type":"string","minLength":1,"maxLength":8192});
    let strings = json!({"type":"array","items":text,"maxItems":32});
    let mut ids = strings.clone();
    ids["minItems"] = json!(1);
    let mut outcomes = strings.clone();
    outcomes["minItems"] = json!(2);
    outcomes["maxItems"] = json!(16);
    json!({
        "type":"object", "additionalProperties":false,
        "required":["symptom","phase","hypotheses","unknowns","next_observation"],
        "properties":{
            "symptom":text,
            "phase":{"type":"string","enum":["investigating","implementing","blocked","resolved","cancelled"]},
            "hypotheses":{"type":"array","minItems":1,"maxItems":16,"items":{
                "type":"object","additionalProperties":false,"required":["id","explanation","status"],
                "properties":{"id":text,"explanation":text,"status":{"type":"string","enum":["open","ruled_out","established"]}}
            }},
            "unknowns":strings,
            "next_observation":{
                "type":"object","additionalProperties":false,
                "required":["id","question","hypothesis_ids","expected_outcomes","obtainable"],
                "properties":{
                    "id":text,"question":text,"hypothesis_ids":ids,"expected_outcomes":outcomes,
                    "obtainable":{"type":"boolean"},"blocker":text,
                    "diagnostic_action":{"type":"object","additionalProperties":false,"required":["tool","input"],
                        "properties":{"tool":text,"input":{}}}
                }
            },
            "finding":{
                "type":"object","additionalProperties":false,
                "required":["kind","observation_id","hypothesis_ids","uncertainty","evidence","conclusion"],
                "properties":{
                    "kind":{"type":"string","enum":["uncertainty_resolved","hypothesis_eliminated","decision_changed","failure_reproduced","cause_established"]},
                    "observation_id":text,"hypothesis_ids":ids,"uncertainty":text,"evidence":text,"conclusion":text
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "investigation_tests.rs"]
pub(crate) mod tests;
