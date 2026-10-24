use codex_features::MULTI_AGENT_DEFAULT_WAIT_TIMEOUT_MS;
use codex_features::MULTI_AGENT_MAX_WAIT_TIMEOUT_MS;
use codex_features::MULTI_AGENT_MIN_WAIT_TIMEOUT_MS;
use codex_protocol::openai_models::ModelPreset;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::multi_agents_common::DEFAULT_SPAWN_AGENT_MODEL;
use super::multi_agents_common::DEFAULT_SPAWN_AGENT_REASONING_EFFORT;


const SPAWN_AGENT_SERVICE_TIER_OVERRIDE_DESCRIPTION: &str =
    "Service tier override for the new agent. Omit unless explicitly requested.";
const SPAWN_AGENT_V2_FULL_HISTORY_OVERRIDE_GUIDANCE: &str = "Full-history forks (`fork_turns=\"all\"`) are disabled by default and do not accept per-call `agent_type`, `model`, or `reasoning_effort` overrides when enabled; set `fork_turns` to `none` or an integer from 1 through 5 when using those fields.";
const MAX_MODEL_OVERRIDES_IN_SPAWN_AGENT_DESCRIPTION: usize = 5;
const MAX_REASONING_EFFORT_CHARS_IN_SPAWN_AGENT_DESCRIPTION: usize = 64;
/// `message` and the aliases the V2 handler accepts for the same task text.
const SPAWN_AGENT_V2_MESSAGE_FIELDS: [&str; 3] = ["message", "prompt", "instructions"];

#[derive(Debug, Clone, Default)]
pub struct SpawnAgentToolOptions {
    pub available_models: Arc<Vec<ModelPreset>>,
    pub agent_type_description: String,
    pub hide_agent_type_model_reasoning: bool,
    pub usage_hint_text: Option<String>,
}

fn spawn_agent_default_guidance() -> String {
    format!(
        "Spawned agents use `{DEFAULT_SPAWN_AGENT_MODEL}` with `{DEFAULT_SPAWN_AGENT_REASONING_EFFORT}` reasoning by default. Omit `model` and `reasoning_effort` to use those defaults; set either field only when an explicit override is needed. If the built-in model is unavailable, use the effective model or provider default. Omitted reasoning effort falls back to a supported model effort."
    )
}

fn spawn_agent_model_override_description() -> String {
    format!(
        "Model override for the new agent. Omit to use `{DEFAULT_SPAWN_AGENT_MODEL}`. If unavailable, the effective model or provider default is used."
    )
}

fn spawn_agent_reasoning_override_description() -> String {
    format!(
        "Reasoning effort override for the new agent. Omit to prefer `{DEFAULT_SPAWN_AGENT_REASONING_EFFORT}`, falling back to a supported effort of the selected model."
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
    let available_models_description = (!options.hide_agent_type_model_reasoning)
        .then(|| spawn_agent_models_description(&options.available_models));
    let default_model_guidance =
        (!options.hide_agent_type_model_reasoning).then(spawn_agent_default_guidance);
    let full_history_override_guidance = (!options.hide_agent_type_model_reasoning)
        .then_some(SPAWN_AGENT_V2_FULL_HISTORY_OVERRIDE_GUIDANCE);
    let mut properties = spawn_agent_common_properties_v2(
        &options.agent_type_description,
        full_history_override_guidance,
    );
    if options.hide_agent_type_model_reasoning {
        hide_spawn_agent_metadata_options(&mut properties);
        // Typed assignments require an explicit typed-capable role even when optional legacy
        // spawn metadata is hidden.
        properties.insert(
            "agent_type".to_string(),
            JsonSchema::string(Some(options.agent_type_description.clone())),
        );
    }
    properties.insert("assignment".to_string(), typed_assignment_schema());
    properties.insert(
        "task_name".to_string(),
        JsonSchema {
            // ECMAScript `$` alone also admits a final newline. Require the
            // absolute end explicitly and retain AgentPath's reserved name.
            pattern: Some(r"^(?!root$)[a-z0-9_]+$(?![\s\S])".to_string()),
            min_length: Some(1),
            ..JsonSchema::string(Some(
                "Task name for the new agent. Use lowercase letters, digits, and underscores; `root` is reserved."
                    .to_string(),
            ))
        },
    );
    // Other agent APIs name this task text `prompt` or `instructions`. Rejecting
    // those spellings failed whole exec cells, so they are published aliases.
    for alias in &SPAWN_AGENT_V2_MESSAGE_FIELDS[1..] {
        properties.insert(
            (*alias).to_string(),
            JsonSchema::string(Some("Alias for `message`.".to_string())).with_encrypted(),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: "spawn_agent".to_string(),
        description: spawn_agent_tool_description_v2(
            available_models_description.as_deref(),
            default_model_guidance.as_deref(),
            full_history_override_guidance,
            options.usage_hint_text,
        ),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema {
            one_of: Some(
                SPAWN_AGENT_V2_MESSAGE_FIELDS
                    .into_iter()
                    .chain(["assignment"])
                    .map(|field| JsonSchema {
                        required: Some(vec![field.to_string()]),
                        ..JsonSchema::object(BTreeMap::new(), None, None)
                    })
                    .collect(),
            ),
            // Keep role admission separate from the exclusive input choice: otherwise
            // message + assignment without agent_type could match the message branch.
            any_of: Some(
                SPAWN_AGENT_V2_MESSAGE_FIELDS
                    .into_iter()
                    .chain(["agent_type"])
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
            spawn_agent_output_schema_v2(options.hide_agent_type_model_reasoning).into(),
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
                "Message text to send to the target agent.".to_string(),
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
        description: "Wait for the next mailbox update or durable typed-task progress. Returns at the first event, not when agents finish: a result that reports only the admission of your own spawn is not progress, so wait again in the same cell. Omit cursor to receive new assignment, gate, receipt, lease, and progress deltas using the caller's automatically retained position; set an explicit cursor only to replay from that position. Does not return the content: a finished agent's answer arrives as a message on your next request. The wait also ends early when new user input is steered into the active turn."
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
    properties.insert("since_revision".into(), JsonSchema::string(Some(
        "Prior display revision for the same root, prefix and compact mode. An exact match returns unchanged:true with empty arrays (retain the previous snapshot); otherwise a full replacement snapshot includes additions and deletions.".into(),
    )));

    ToolSpec::Function(ResponsesApiTool {
        name: "list_agents".to_string(),
        description:
            "List live agents in the current root thread tree. For routine polling use compact:true and reuse since_revision to avoid repeating unchanged evidence. These revisions do not certify source freshness or task completion."
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
                            "description": "Last known status of the agent.",
                            "allOf": [agent_status_output_schema()]
                        },
                        "last_task_message": {
                            "type": ["string", "null"],
                            "description": "Most recent user or inter-agent instruction received by the agent, when available."
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
            "task_text_recovery": { "type": ["string", "null"]
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
    agent_type_description: &str,
    full_history_override_guidance: Option<&str>,
) -> BTreeMap<String, JsonSchema> {
    let full_history_override_guidance = full_history_override_guidance
        .map(|guidance| format!(" {guidance}"))
        .unwrap_or_default();
    BTreeMap::from([
        (
            "message".to_string(),
            JsonSchema::string(Some(
                "Complete, self-contained task. The new agent sees none of this conversation unless `fork_turns` is set, so include the paths, facts and stop condition it needs. Admitted with a repository-wide diagnostic claim that records coordination risk without blocking overlapping work."
                    .to_string(),
            ))
            .with_encrypted(),
        ),
        (
            "agent_type".to_string(),
            JsonSchema::string(Some(agent_type_description.to_string())),
        ),
        (
            "fork_turns".to_string(),
            JsonSchema::string(Some(format!(
                "Optional fork mode. Omitted or `none` creates a lineage-preserving TaskCapsule fork with no parent conversation. Use an integer string from 1 through 5 for a history-bearing fork. Full history (`all`) is disabled by default.{full_history_override_guidance}"
            ))),
        ),
        (
            "model".to_string(),
            JsonSchema::string(Some(spawn_agent_model_override_description())),
        ),
        (
            "reasoning_effort".to_string(),
            JsonSchema::string(Some(spawn_agent_reasoning_override_description())),
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

fn typed_assignment_schema() -> JsonSchema {
    let criterion = JsonSchema::object(
        BTreeMap::from([
            (
                "evidence_policy".to_string(),
                criterion_evidence_policy_schema(),
            ),
            (
                "id".to_string(),
                JsonSchema::string(Some("Stable acceptance-criterion id.".to_string())),
            ),
            (
                "text".to_string(),
                JsonSchema::string(Some("Required observable outcome.".to_string())),
            ),
        ]),
        Some(vec!["id".to_string(), "text".to_string()]),
        Some(false.into()),
    );
    let repo_scope = JsonSchema::object(
        BTreeMap::from([
            (
                "path".to_string(),
                JsonSchema::string(Some("Repository-relative path.".to_string())),
            ),
            (
                "recursive".to_string(),
                JsonSchema::boolean(Some(
                    "Whether this scope includes descendants of path.".to_string(),
                )),
            ),
        ]),
        Some(vec!["path".to_string(), "recursive".to_string()]),
        Some(false.into()),
    );
    let string_array = |description: &str| {
        JsonSchema::array(JsonSchema::string(None), Some(description.to_string()))
    };
    let relation = JsonSchema::object(
        BTreeMap::from([
            (
                "kind".to_string(),
                JsonSchema::string_enum(
                    vec![json!("review"), json!("verification"), json!("integration")],
                    Some("How this assignment relates to its targets.".to_string()),
                ),
            ),
            (
                "target_assignment_ids".to_string(),
                string_array("UUIDv7 assignments reviewed, verified, or integrated."),
            ),
        ]),
        Some(vec![
            "kind".to_string(),
            "target_assignment_ids".to_string(),
        ]),
        Some(false.into()),
    );
    let architecture_contract_ref = JsonSchema::object(
        BTreeMap::from([
            (
                "architect_assignment_id".to_string(),
                JsonSchema::string(Some(
                    "Architect assignment owning the sealed receipt.".to_string(),
                )),
            ),
            (
                "architect_attempt_id".to_string(),
                JsonSchema::string(Some(
                    "Exact successful Architect attempt owning the receipt.".to_string(),
                )),
            ),
            (
                "contract_version".to_string(),
                JsonSchema::integer(Some("ArchitectureContractV1 version.".to_string())),
            ),
            (
                "contract_sha256".to_string(),
                JsonSchema::string(Some(
                    "Canonical SHA-256 from the immutable Architect receipt.".to_string(),
                )),
            ),
        ]),
        Some(vec![
            "architect_assignment_id".to_string(),
            "architect_attempt_id".to_string(),
            "contract_version".to_string(),
            "contract_sha256".to_string(),
        ]),
        Some(false.into()),
    );
    let file_handle = JsonSchema::object(
        BTreeMap::from([
            (
                "kind".to_string(),
                JsonSchema::string_enum(
                    vec![json!("file")],
                    Some("File handle variant.".to_string()),
                ),
            ),
            (
                "path".to_string(),
                JsonSchema::string(Some(
                    "Repository-relative existing file or missing intended file; directories are rejected."
                        .to_string(),
                )),
            ),
        ]),
        Some(vec!["kind".to_string(), "path".to_string()]),
        Some(false.into()),
    );
    let symbol_handle = JsonSchema::object(
        BTreeMap::from([
            (
                "kind".to_string(),
                JsonSchema::string_enum(
                    vec![json!("symbol")],
                    Some("Symbol handle variant.".to_string()),
                ),
            ),
            (
                "path".to_string(),
                JsonSchema::string(Some(
                    "Repository-relative existing file or missing intended file; directories are rejected."
                        .to_string(),
                )),
            ),
            (
                "symbol".to_string(),
                JsonSchema::string(Some(
                    "Stable compiler-neutral symbol locator; line spans are not accepted."
                        .to_string(),
                )),
            ),
        ]),
        Some(vec![
            "kind".to_string(),
            "path".to_string(),
            "symbol".to_string(),
        ]),
        Some(false.into()),
    );
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "objective".to_string(),
                JsonSchema::string(Some("Concrete objective; for audits/reviews, include the task type and category inclusions/exclusions.".to_string())),
            ),
            (
                "acceptance_criteria".to_string(),
                JsonSchema::array(
                    criterion,
                    Some("Observable outcomes required for completion; for audits/reviews, include the assigned detection criteria.".to_string()),
                ),
            ),
            (
                "read_scope".to_string(),
                JsonSchema::array(
                    repo_scope.clone(),
                    Some("Repository paths the agent is expected to inspect.".to_string()),
                ),
            ),
            (
                "write_scope".to_string(),
                JsonSchema::array(
                    repo_scope,
                    Some(
                        "Intended repository write scopes; overlaps are advisory and require coordination. Use an empty array for read-only roles."
                            .to_string(),
                    ),
                ),
            ),
            (
                "stop_condition".to_string(),
                JsonSchema::string(Some(
                    "Condition that ends the assignment without further work.".to_string(),
                )),
            ),
            (
                "dependencies".to_string(),
                string_array("Successful sealed assignment ids required before this task."),
            ),
            (
                "risk_hints".to_string(),
                string_array(
                    "Known risks the agent must account for. Use a recognized risk domain \
                     (concurrency, unsafe, lifecycle, persistence, schema, protocol, security, or \
                     installation), or prefix a configured risk with path: or contract:.",
                ),
            ),
            (
                "required_evidence".to_string(),
                string_array("Completion evidence: inspect:<normalized repository-relative file> for a complete native source read, or exact resolved command argv (JSON array or shell-quoted) for native validation. Listing, busy, zero-test, failed and cancelled runs do not count. See criterion evidence_policy for applicability and freshness."),
            ),
            (
                "prohibited_changes".to_string(),
                string_array("User and assignment restrictions on edits and execution."),
            ),
            (
                "contract_claims".to_string(),
                string_array(
                    "Named behavioral contracts touched by this assignment; overlaps are advisory metadata.",
                ),
            ),
            (
                "relevant_handles".to_string(),
                JsonSchema::array(
                    JsonSchema::any_of(vec![file_handle, symbol_handle], None),
                    Some(
                        "Explicit non-recursive file or symbol handles included in a typed TaskCapsule. Paths must be covered by read_scope or write_scope; duplicate handles are rejected."
                            .to_string(),
                    ),
                ),
            ),
            (
                "workspace_strategy".to_string(),
                JsonSchema::string_enum(
                    vec![json!("auto"), json!("shared"), json!("isolated")],
                    Some(
                        "Use shared (what auto selects) for the common workspace, including coordinated overlap, and isolated for deliberate competing implementations."
                            .to_string(),
                    ),
                ),
            ),
            ("relation".to_string(), relation),
            (
                "architecture_contract_ref".to_string(),
                architecture_contract_ref,
            ),
        ]),
        Some(vec![
            "objective".to_string(),
            "acceptance_criteria".to_string(),
            "write_scope".to_string(),
            "stop_condition".to_string(),
        ]),
        Some(false.into()),
    );
    schema.description = Some(
        "Durable typed assignment. Use either assignment or legacy message, never both. Typed assignments require an explicit built-in agent_type: architect, explorer, worker, reviewer, verifier, or integrator. Role requirements: architect/explorer need non-empty read_scope and empty required_evidence; worker needs non-empty write_scope and required_evidence; reviewer needs empty required_evidence; verifier/integrator need non-empty required_evidence. Architect, explorer, reviewer and verifier need an empty write_scope; integrator needs a non-empty one. Architect, explorer and worker declare no relation; reviewer needs a review relation and verifier a verification relation with exactly one target that is also in dependencies; integrator needs an integration relation whose distinct targets are all in dependencies. With omitted or `none` fork_turns, one structured TaskCapsule replaces the plaintext typed bootstrap message and is the sole initial task input."
            .to_string(),
    );
    schema
}

fn hide_spawn_agent_metadata_options(properties: &mut BTreeMap<String, JsonSchema>) {
    properties.remove("agent_type");
    properties.remove("model");
    properties.remove("reasoning_effort");
    properties.remove("service_tier");
}


// Keep tool/assignment mechanics here; audit policy belongs to the base instructions,
// and role/coordination guidance belongs to config/mod.rs. See AGENTS.md for ownership.
fn spawn_agent_tool_description_v2(
    available_models_description: Option<&str>,
    default_model_guidance: Option<&str>,
    full_history_override_guidance: Option<&str>,
    usage_hint_text: Option<String>,
) -> String {
    let agent_role_guidance = available_models_description.unwrap_or_default();
    let default_model_guidance = default_model_guidance.unwrap_or_default();
    let full_history_override_guidance = full_history_override_guidance.unwrap_or_default();

    let tool_description = format!(
        r#"
        {agent_role_guidance}
        Spawns an agent to work on the specified task. If your current task is `/root/task1` and you spawn_agent with task_name "task_3" the agent will have canonical task name `/root/task1/task_3`.
You are then able to refer to this agent as `task_3` or `/root/task1/task_3` interchangeably. However an agent `/root/task2/task_3` would only be able to communicate with this agent via its canonical name `/root/task1/task_3`.
Registered leaf names take precedence over compact UUIDs; use a hyphenated UUID to select a concrete thread unambiguously.
The spawned agent receives a durable task binding.
{default_model_guidance}
Only call this tool for a concrete, bounded subtask that can run independently alongside useful local work; otherwise continue locally.
Apply the base instructions for audit/review scope, verification, validation, and completion, and the team guidance for ownership and coordination.
Size assignments to fit the worker's actual context capacity, reserving room for reasoning, evidence, and reporting. Partition by cohesive subsystems and execution paths rather than raw file size alone.
Legacy `message` assignments must include the task type, category inclusions/exclusions, detection criteria, and user restrictions; typed assignments use the fields below.
Launch authorized independent assignments in the same batch rather than awaiting each spawn in a loop. In exec, use Promise.allSettled for a bounded batch and retain every success and failure before proceeding; a failed sibling does not authorize respawning successful agents.
Sequence only real dependencies or shared-resource conflicts. Do not add a model round merely to acknowledge a completed agent; integrate its result with the next substantive step or final response.
It will be able to send you and other running agents messages, and its final answer will be provided to you when it finishes.
Use `assignment` for explicitly scoped typed coordination or `message` for a compatibility task admitted with a repository-wide diagnostic claim; exactly one is required. Both forms are durably admitted, record overlap without rejecting it, and return an `assignment_id`. Typed assignments require an explicit built-in `agent_type`: `architect`, `explorer`, `worker`, `reviewer`, `verifier`, or `integrator`.
The `reviewer` profile requires a typed assignment with a review relation; it cannot be used with the compatibility `message` form.

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

fn spawn_agent_models_description(models: &[ModelPreset]) -> String {
    let visible_models: Vec<&ModelPreset> = models
        .iter()
        .filter(|model| model.show_in_picker)
        .take(MAX_MODEL_OVERRIDES_IN_SPAWN_AGENT_DESCRIPTION)
        .collect();
    if visible_models.is_empty() {
        return "No picker-visible model overrides are currently loaded.".to_string();
    }

    let model_descriptions = visible_models
        .into_iter()
        .map(|model| {
            let default_reasoning_effort = &model.default_reasoning_effort;
            let efforts = model
                .supported_reasoning_efforts
                .iter()
                .map(|preset| {
                    let effort = preset.effort.as_str();
                    let effort = match effort
                        .char_indices()
                        .nth(MAX_REASONING_EFFORT_CHARS_IN_SPAWN_AGENT_DESCRIPTION)
                    {
                        Some((index, _)) => &effort[..index],
                        None => effort,
                    };
                    if &preset.effort == default_reasoning_effort {
                        format!("{effort} (catalog default)")
                    } else {
                        effort.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let reasoning_efforts_suffix = if efforts.is_empty() {
                String::new()
            } else {
                format!(" Reasoning efforts: {efforts}.")
            };
            let service_tiers = model
                .service_tiers
                .iter()
                .map(|tier| tier.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let service_tiers_suffix = if service_tiers.is_empty() {
                String::new()
            } else {
                format!(" Service tiers: {service_tiers}.")
            };
            let model_slug = &model.model;
            let description = &model.description;
            format!(
                "- `{model_slug}`: {description}{reasoning_efforts_suffix}{service_tiers_suffix}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Available model overrides (optional; `{DEFAULT_SPAWN_AGENT_MODEL}` is the spawn default):\n{model_descriptions}"
    )
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

#[cfg(test)]
#[path = "multi_agents_spec_tests.rs"]
mod tests;
