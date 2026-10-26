use codex_features::MULTI_AGENT_DEFAULT_WAIT_TIMEOUT_MS;
use codex_features::MULTI_AGENT_MAX_WAIT_TIMEOUT_MS;
use codex_features::MULTI_AGENT_MIN_WAIT_TIMEOUT_MS;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

use super::multi_agents_common::DEFAULT_SPAWN_AGENT_MODEL;
use super::multi_agents_common::DEFAULT_SPAWN_AGENT_REASONING_EFFORT;


const SPAWN_AGENT_SERVICE_TIER_OVERRIDE_DESCRIPTION: &str =
    "Service tier override for the new agent. Omit unless explicitly requested.";
const SPAWN_AGENT_V2_FULL_HISTORY_OVERRIDE_GUIDANCE: &str = "Full-history forks (`fork_turns=\"all\"`) are disabled by default; prefer `none` or an integer from 1 through 5.";
/// `message` and the aliases the V2 handler accepts for the same task text.
/// Keep these plaintext: durable objectives are rendered inside ordinary capsule
/// or history-bootstrap text, not delivered as Responses EncryptedContent.
const SPAWN_AGENT_V2_MESSAGE_FIELDS: [&str; 3] = ["message", "prompt", "instructions"];

#[derive(Debug, Clone, Default)]
pub struct SpawnAgentToolOptions {
    pub hide_agent_metadata: bool,
    pub usage_hint_text: Option<String>,
}

fn spawn_agent_default_guidance() -> String {
    format!(
        "All subagents are generic and use `{DEFAULT_SPAWN_AGENT_MODEL}` with `{DEFAULT_SPAWN_AGENT_REASONING_EFFORT}` reasoning. Describe the job and its restrictions in `message`; there are no role, model, or reasoning overrides. If the built-in model is unavailable, the effective model or provider default is used with a supported reasoning effort."
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitAgentTimeoutOptions {
    pub default_timeout_ms: i64,
    pub min_timeout_ms: i64,
    pub max_timeout_ms: i64,
}

impl Default for WaitAgentTimeoutOptions {
    fn default() -> Self {
        Self {
            default_timeout_ms: MULTI_AGENT_DEFAULT_WAIT_TIMEOUT_MS,
            min_timeout_ms: MULTI_AGENT_MIN_WAIT_TIMEOUT_MS,
            max_timeout_ms: MULTI_AGENT_MAX_WAIT_TIMEOUT_MS,
        }
    }
}


pub fn create_spawn_agent_tool_v2(options: SpawnAgentToolOptions) -> ToolSpec {
    let mut properties = spawn_agent_common_properties_v2(
        Some(SPAWN_AGENT_V2_FULL_HISTORY_OVERRIDE_GUIDANCE),
    );
    if options.hide_agent_metadata {
        hide_spawn_agent_metadata_options(&mut properties);
    }
    properties.insert(
        "task_name".to_string(),
        JsonSchema {
            pattern: Some(r"^(?!root$)[a-z0-9_]+$(?![\s\S])".to_string()),
            min_length: Some(1),
            ..JsonSchema::string(Some(
                "Task name for the new agent. Use lowercase letters, digits, and underscores; `root` is reserved."
                    .to_string(),
            ))
        },
    );
    for alias in &SPAWN_AGENT_V2_MESSAGE_FIELDS[1..] {
        properties.insert(
            (*alias).to_string(),
            JsonSchema::string(Some("Alias for `message`.".to_string())),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: "spawn_agent".to_string(),
        description: spawn_agent_tool_description_v2(options.usage_hint_text),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema {
            one_of: Some(
                SPAWN_AGENT_V2_MESSAGE_FIELDS
                    .into_iter()
                    .map(|field| JsonSchema {
                        required: Some(vec![field.to_string()]),
                        ..JsonSchema::object(BTreeMap::new(), None, None)
                    })
                    .collect(),
            ),
            ..JsonSchema::object(
                properties,
                Some(vec!["task_name".to_string()]),
                Some(false.into()),
            )
        },
        output_schema: Some(
            spawn_agent_output_schema_v2(options.hide_agent_metadata).into(),
        ),
    })
}


pub fn create_send_message_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "targets".to_string(),
            JsonSchema {
                min_items: Some(1),
                ..JsonSchema::array(
                    JsonSchema::string(None),
                    Some("Explicit recipients for the same message. Supply either target or a non-empty targets list; each recipient returns queued or failed.".to_string()),
                )
            },
        ),
        (
            "target".to_string(),
            JsonSchema::string(Some(
                "Relative or canonical task name to message, as `task_name` from spawn_agent."
                    .to_string(),
            )),
        ),
        // `spawn_agent` takes and returns this value under the name `task_name`,
        // so a caller carrying that field forward is using this tool's own
        // vocabulary. Accepting it avoids failing the whole cell, along with any
        // unrelated work batched before the call, over a spelling.
        ("task_name".to_string(), task_name_alias_schema()),
        (
            "message".to_string(),
            JsonSchema::string(Some(
                "Message text to queue on the target agent.".to_string(),
            ))
            .with_encrypted(),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "send_message".to_string(),
        description: "Queue information for a child or peer, or the same information for explicit targets, without starting a new turn. Sub-agents cannot message ancestors: finish with one final answer including any blockers; the host reports the terminal outcome automatically. Do not send progress updates or request acknowledgment. Use followup_task for new assignments, corrections requiring action, or urgent work that must start."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema {
            one_of: Some(vec![
                JsonSchema {
                    // Agreeing aliases remain valid together. Equality is checked at runtime.
                    any_of: Some(["target", "task_name"].into_iter().map(|field| JsonSchema {
                        required: Some(vec![field.to_string()]),
                        ..JsonSchema::object(BTreeMap::new(), None, None)
                    }).collect()),
                    ..JsonSchema::default()
                },
                JsonSchema::object(BTreeMap::new(), Some(vec!["targets".to_string()]), None),
            ]),
            ..JsonSchema::object(properties, Some(vec!["message".to_string()]), Some(false.into()))
        },
        output_schema: None,
    })
}

pub fn create_followup_task_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "target".to_string(),
            JsonSchema::string(Some(
                "Agent id, or canonical task name as `task_name` from spawn_agent, to send a follow-up task to."
                    .to_string(),
            )),
        ),
        (
            "message".to_string(),
            JsonSchema::string(Some(
                "New task or correction and necessary evidence; reference unchanged context instead of resending the prior assignment.".to_string(),
            ))
            .with_encrypted(),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "followup_task".to_string(),
        description: "Send a new assignment, correction requiring action, or urgent intervention to an existing non-root agent; trigger a turn if idle. If running, deliver promptly at message boundaries while sampling, or after the pending tool call completes. Use send_message for informational updates that need no new turn; ask for clarification when needed. A typed (`assignment`) agent accepts a follow-up only while its attempt is active; once its receipt is sealed, spawn a new assignment instead."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["target".to_string(), "message".to_string()]), Some(false.into())),
        output_schema: None,
    })
}



pub fn create_wait_agent_tool_v2(options: WaitAgentTimeoutOptions) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "wait_agent".to_string(),
        description: "Wait for mailbox input or durable task progress. Returns at the first event, not when agents finish. On a mailbox, deferred-result or new-input wake, return from the exec cell; empty typed_deltas is not a reason to wait again. Admission-only bookkeeping is folded internally. Omit cursor to receive new deltas from the retained position; supply one only to replay. Does not return the content: a finished agent's answer arrives as a message on your next request."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: wait_agent_tool_parameters_v2(options),
        output_schema: Some(wait_output_schema_v2().into()),
    })
}

pub fn create_list_agents_tool() -> ToolSpec {
    let mut properties = BTreeMap::from([(
        "path_prefix".to_string(),
        JsonSchema::string(Some(
            "Task-path prefix filter; trailing slashes are ignored. Omit to list all live agents."
                .to_string(),
        )),
    )]);
    properties.insert("compact".into(), JsonSchema::boolean(Some(
        "Omit repeated task-message previews; status, results, failures and required actions remain complete. Default false.".into(),
    )));
    properties.insert("status_only".into(), JsonSchema::boolean(Some(
        "Opt in to liveness polling without task previews or completed report/surfaced-result bodies. Completed statuses become distinct completed/completed_with_surface strings with exact immutable completed_report_recovery arguments. If report retention fails, completed reports remain inline. Errors, stopped reports, blockers and required actions remain complete. Default false.".into(),
    )));
    properties.insert("since_revision".into(), JsonSchema::string(Some(
        "Prior display revision for the same root, prefix, compact and status_only modes. An exact match returns unchanged:true with empty arrays (retain the previous snapshot); otherwise a replacement snapshot for the requested view includes additions and deletions. Changes to omitted reports also change the revision.".into(),
    )));

    ToolSpec::Function(ResponsesApiTool {
        name: "list_agents".to_string(),
        description:
            "List live agents in the current root thread tree. For routine liveness polling use status_only:true and reuse since_revision; terminal reports are delivered automatically. Full details remain the default, and compact:true alone preserves final results. Recover omitted reports with completed_report_recovery through the advertised read_tool_output entrypoint; for a current full snapshot request status_only:false with a targeted path_prefix and no since_revision before waking the worker just to resend text. Revisions and completed lifecycle statuses do not certify source freshness or task completion."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: Some(list_agents_output_schema().into()),
    })
}


pub fn create_interrupt_agent_tool_v2() -> ToolSpec {
    let properties = BTreeMap::from([(
        "target".to_string(),
        JsonSchema::string(Some(
            "Agent id or canonical task name to interrupt (from spawn_agent).".to_string(),
        )),
    )]);

    ToolSpec::Function(ResponsesApiTool {
        name: "interrupt_agent".to_string(),
        description: "Interrupt an agent's current turn, if any, and return its previous status. The agent remains available for messages and follow-up tasks.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["target".to_string()]), Some(false.into())),
        output_schema: Some(agent_previous_status_output_schema(
            "The agent status observed before the interrupt request was handled.",
        ).into()),
    })
}

fn agent_status_output_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "string",
                "enum": ["pending_init", "running", "interrupted", "shutdown", "not_found"]
            },
            {
                "type": "object",
                "properties": {
                    "completed": {
                        "type": ["string", "null"]
                    }
                },
                "required": ["completed"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "completed_with_surface": {
                        "type": "object",
                        "properties": {
                            "last_agent_message": {
                                "type": ["string", "null"]
                            },
                            "surfaced_result": {
                                "type": "object",
                                "properties": {
                                    "adapter": {"type": "string"},
                                    "value": {},
                                    "canonicalMessage": {"type": "string"}
                                },
                                "required": ["adapter", "value"],
                                "additionalProperties": false
                            }
                        },
                        "required": ["last_agent_message", "surfaced_result"],
                        "additionalProperties": false
                    }
                },
                "required": ["completed_with_surface"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "stopped": {
                        "type": "object",
                        "properties": {
                            "last_agent_message": {"type": ["string", "null"]},
                            "stop_cause": {"type": "string", "enum": ["subagent_context_budget"]}
                        },
                        "required": ["last_agent_message", "stop_cause"],
                        "additionalProperties": false
                    }
                },
                "required": ["stopped"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "errored": {
                        "type": "string"
                    }
                },
                "required": ["errored"],
                "additionalProperties": false
            }
        ]
    })
}


pub(super) fn spawn_agent_output_schema_v2(hide_agent_metadata: bool) -> Value {
    let integration_plan = json!({
        "type": "string",
        "enum": ["single_writer", "root_owned", "typed_integrator_required"]
    });
    let mut spawned = json!({
        "type": "object",
        "properties": {
            "task_name": {"type": "string"},
            "assignment_id": {"type": "string"},
            "integration_plan": integration_plan
        },
        "required": ["task_name", "assignment_id", "integration_plan"],
        "additionalProperties": false
    });
    if !hide_agent_metadata {
        spawned["properties"]["nickname"] = json!({"type": ["string", "null"]});
        spawned["required"] = json!(["task_name", "nickname", "assignment_id", "integration_plan"]);
    }
    let reused = json!({
        "type": "object",
        "properties": {
            "task_name": {"type": "string"},
            "assignment_id": {"type": "string"},
            "attempt_id": {"type": "string"},
            "agent_path": {"type": ["string", "null"]},
            "thread_id": {"type": ["string", "null"]},
            "status": {"type": "string"},
            "receipt_available": {"type": "boolean"},
            "integration_plan": integration_plan,
            "reused": {"const": true}
        },
        "required": ["task_name", "assignment_id", "attempt_id", "agent_path", "thread_id", "status", "receipt_available", "integration_plan", "reused"],
        "additionalProperties": false
    });
    json!({"oneOf": [spawned, reused]})
}


fn list_agents_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agents": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "agent_name": {
                            "type": "string",
                            "description": "Canonical task name for the agent when available, otherwise the agent id."
                        },
                        "agent_status": {
                            "description": "Last known lifecycle status, not task acceptance. Only status_only:true replaces completed bodies with distinct status strings; see completed_report_recovery for exact full recovery.",
                            "oneOf": [
                                agent_status_output_schema(),
                                { "type": "string", "enum": ["completed", "completed_with_surface"] }
                            ]
                        },
                        "last_task_message": {
                            "type": ["string", "null"],
                            "description": "Most recent user or inter-agent instruction received by the agent, when available; null in compact or status_only views."
                        },
                        "runtime_loaded": {
                            "type": "boolean",
                            "description": "Whether the agent's thread is currently loaded in this process."
                        }
                    },
                    "required": ["agent_name", "agent_status", "last_task_message", "runtime_loaded"],
                    "additionalProperties": false
                },
                "description": "Live agents visible in the current root thread tree."
            },
            "typed_tasks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": true
                },
                "description": "Durable typed-task status including workspace identity and strategy, progress, lease, pending gates, stale reason, and next action."
            },
            "unresolved_descendants": {
                "type": "array",
                "items": { "type": "array", "items": { "type": "string" }, "minItems": 2, "maxItems": 2 }
            },
            "revision": { "type": "string", "description": "Opaque display identity, not a source or acceptance revision." },
            "unchanged": { "type": "boolean", "description": "If true retain the prior snapshot; empty arrays do not revoke existing failures or agents." },
            "task_text_recovery": { "type": ["string", "null"] },
            "completed_report_recovery": {
                "type": "object",
                "description": "Exact immutable completed statuses from this revision, including final messages and authoritative surfaced values. Invoke tool through its advertised namespace with these arguments and follow any continuation; the artifact is caller-thread scoped and survives later report changes or agent deletion.",
                "properties": {
                    "tool": { "type": "string", "enum": ["read_tool_output"] },
                    "arguments": {
                        "type": "object",
                        "properties": {
                            "artifact_id": { "type": "string" },
                            "selectors": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "const": "json_pointer" },
                                        "pointer": { "const": "/agents" }
                                    },
                                    "required": ["kind", "pointer"],
                                    "additionalProperties": false
                                },
                                "minItems": 1,
                                "maxItems": 1
                            }
                        },
                        "required": ["artifact_id", "selectors"],
                        "additionalProperties": false
                    },
                    "canonical_sha256": { "type": "string" },
                    "canonical_bytes": { "type": "integer" }
                },
                "required": ["tool", "arguments", "canonical_sha256", "canonical_bytes"],
                "additionalProperties": false
            }
        },
        "required": ["agents", "typed_tasks"],
        "additionalProperties": false
    })
}



fn wait_output_schema_v2() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Brief wait summary without the agent's final content."
            },
            "timed_out": {
                "type": "boolean",
                "description": "Whether the wait call returned because no mailbox update arrived before the timeout."
            },
            "cursor": {
                "type": ["string", "null"],
                "description": "Durable typed-task wake cursor to pass to the next wait."
            },
            "typed_deltas": {
                "type": "array",
                "description": "Durable typed-task deltas after the supplied cursor.",
                "items": {
                    "type": "object",
                    "additionalProperties": true
                }
            },
            "truncated_count": {
                "type": "integer",
                "description": "Number of durable deltas omitted because of retention or page limits."
            },
            "nudged_assignment_ids": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Stalled live assignments that received their one durable no-progress nudge."
            },
            "previous_delivery": {
                "type": "object",
                "description": "Recovery receipt for the previous automatic-cursor wait result; if that result was not received, read artifact_id with read_tool_output.",
                "properties": {
                    "cursor": {"type": ["string", "null"]},
                    "artifact_id": {"type": "string"},
                    "thread_id": {"type": "string"},
                    "recovery_tool": {"type": "string", "enum": ["read_tool_output"]}
                },
                "required": ["cursor", "artifact_id", "thread_id", "recovery_tool"],
                "additionalProperties": false
            }
        },
        "required": ["message", "timed_out", "cursor", "typed_deltas", "truncated_count", "nudged_assignment_ids"],
        "additionalProperties": false
    })
}

fn agent_previous_status_output_schema(previous_status_description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "previous_status": {
                "description": previous_status_description,
                "allOf": [agent_status_output_schema()]
            }
        },
        "required": ["previous_status"],
        "additionalProperties": false
    })
}



fn spawn_agent_common_properties_v2(
    full_history_override_guidance: Option<&str>,
) -> BTreeMap<String, JsonSchema> {
    let full_history_override_guidance = full_history_override_guidance
        .map(|guidance| format!(" {guidance}"))
        .unwrap_or_default();
    BTreeMap::from([
        (
            "message".to_string(),
            JsonSchema::string(Some(
                "Complete, self-contained task with the needed paths, facts, restrictions, evidence references and stop condition, not a copied transcript. The new agent sees none of this conversation unless `fork_turns` is set. Admitted with a repository-wide diagnostic claim that records coordination risk without blocking overlapping work."
                    .to_string(),
            )),
        ),
        (
            "fork_turns".to_string(),
            JsonSchema::string(Some(format!(
                "Optional fork mode. Omitted or `none` creates a lineage-preserving TaskCapsule fork with no parent conversation; prefer this for self-contained tasks. Use the smallest sufficient integer string from 1 through 5 only when prior conversation is needed. Full history (`all`) is disabled by default.{full_history_override_guidance}"
            ))),
        ),
        (
            "service_tier".to_string(),
            JsonSchema::string(Some(
                SPAWN_AGENT_SERVICE_TIER_OVERRIDE_DESCRIPTION.to_string(),
            )),
        ),
    ])
}

/// Accepts the recipient under the name `spawn_agent` uses for the same value.
///
/// `spawn_agent` both takes and returns it as `task_name`, so carrying that
/// field into `send_message` is a natural reading of this API rather than an
/// invented argument. `additionalProperties: false` turned it into a hard
/// failure of the entire exec cell, discarding any work the cell had already
/// completed, so the alias is published instead of rejected.
fn task_name_alias_schema() -> JsonSchema {
    JsonSchema::string(Some(
        "Alias for target, accepting spawn_agent's `task_name` directly.".to_string(),
    ))
}

pub(crate) fn criterion_evidence_policy_schema() -> JsonSchema {
    JsonSchema::object(BTreeMap::from([
        ("kind".into(), JsonSchema::string_enum(vec![json!("report_only"), json!("source_inspection"), json!("validation_execution")], Some("Declared assurance, not inferred coverage. Omission of the entire policy leaves a legacy claim unverified.".into()))),
        ("required_evidence".into(), JsonSchema::string(Some("Exact assignment required_evidence entry supporting this criterion; required for inspection/execution, omitted for report_only.".into()))),
        ("freshness".into(), JsonSchema::string_enum(vec![json!("current"), json!("historical")], Some("Defaults to current. Current source acquisition is rehashed. Current validation_execution is rejected at assignment/amendment admission because trusted execution input binding is unavailable; another passing run cannot satisfy it. Choose historical only for an explicitly historical execution requirement, never to weaken a current-correctness requirement.".into()))),
    ]), Some(vec!["kind".into()]), Some(false.into()))
}



fn hide_spawn_agent_metadata_options(properties: &mut BTreeMap<String, JsonSchema>) {
    properties.remove("service_tier");
}


// Keep tool/assignment mechanics here; audit policy belongs to the base instructions,
// and role/coordination guidance belongs to config/mod.rs. See AGENTS.md for ownership.
fn spawn_agent_tool_description_v2(
    usage_hint_text: Option<String>,
) -> String {
    let default_model_guidance = spawn_agent_default_guidance();
    let full_history_override_guidance = SPAWN_AGENT_V2_FULL_HISTORY_OVERRIDE_GUIDANCE;

    let tool_description = format!(
        r#"
        Spawns an agent to work on the specified task. Under `/root/task1`, task_name "task_3" creates `/root/task1/task_3`; its parent can use either name, other branches must use the full path.
Registered leaf names take precedence over compact UUIDs; use a hyphenated UUID to select a concrete thread unambiguously.
The spawned agent receives a durable task binding.
{default_model_guidance}
Only call this tool for a concrete, bounded subtask that can run independently alongside useful local work; otherwise continue locally.
Apply the base instructions for audit/review scope, verification, validation, and completion, and the team guidance for ownership and coordination.
Fit assignments to actual worker context, reserving reasoning/evidence/reporting space; partition by cohesive subsystems and execution paths, not just file size.
The `message` must include the task type, intended read/write scope, category inclusions/exclusions, detection criteria, user restrictions, required evidence, and stop condition. Read-only tasks remain read-only by instruction; no specialized profile is selected.
Launch authorized independent assignments in the same batch, not a serial spawn loop. In exec, use bounded Promise.allSettled and retain every success and failure; failed siblings do not justify respawning successes.
Sequence only real dependencies or shared-resource conflicts. Integrate completions with the next substantive step/final, not an acknowledgment-only model round.
It can message its children and peers, but cannot message ancestors. Its final answer and terminal outcome are delivered to its parent automatically when it finishes.
Supply `message` (or its `prompt`/`instructions` alias). Each task is durably tracked with a repository-wide diagnostic claim and returns an `assignment_id`; overlap is recorded without rejecting it. The host records the final outcome automatically, without a role-specific receipt workflow.

    {full_history_override_guidance}
Omitted `fork_turns` and `fork_turns="none"` create a lineage-preserving TaskCapsule fork: the capsule is the sole structured bootstrap and no parent conversation or conversation-derived capability selection is inherited. Integers from 1 through 5 retain bounded history. `fork_turns="all"` is disabled by default and is only available with the configuration escape hatch."#
    );

    if let Some(usage_hint_text) = usage_hint_text {
        return format!(
            r#"
        {tool_description}
{usage_hint_text}"#
        );
    }
    tool_description
}




fn wait_agent_tool_parameters_v2(options: WaitAgentTimeoutOptions) -> JsonSchema {
    let properties = BTreeMap::from([
        (
            "timeout_ms".to_string(),
            JsonSchema {
                ..JsonSchema::integer(Some(format!(
                    "Explicit caller deadline in milliseconds. Omit to keep waiting; the default {} ms interval is internal maintenance cadence only and does not return an unchanged result. Values below {} are ignored and wait without a deadline; values above {} are capped. Use list_agents or get_agent_task for an immediate status snapshot.",
                    options.default_timeout_ms, options.min_timeout_ms, options.max_timeout_ms,
                )))
            },
        ),
        (
            "cursor".to_string(),
            JsonSchema::string(Some(
                "Omit to continue from the caller's automatically retained cursor. Set a cursor returned by wait_agent only to replay from that position."
                    .to_string(),
            )),
        ),
    ]);

    JsonSchema::object(properties, /*required*/ None, Some(false.into()))
}

