use anyhow::Result;
use codex_core::CodexThread;
use codex_core::StartThreadOptions;
use codex_core::ThreadConfigSnapshot;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use test_case::test_case;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const SPAWN_CALL_ID: &str = "spawn-call-1";
const RECEIPT_CALL_ID: &str = "receipt-call-1";
const WAIT_CALL_ID: &str = "wait-call-1";
const WAIT_FOR_RECEIPT_CALL_ID: &str = "wait-call-for-receipt";
const WAIVE_REVIEW_GATE_CALL_ID: &str = "waive-review-gate-call";
const WAIVE_VERIFICATION_GATE_CALL_ID: &str = "waive-verification-gate-call";
const RESUMED_GET_TASK_CALL_ID: &str = "resumed-get-task-call";
const STALE_RECEIPT_CALL_ID: &str = "receipt-call-stale";
const STALE_GET_TASK_CALL_ID: &str = "get-task-call-stale";
const STALE_SET_GATE_CALL_ID: &str = "set-gate-call-stale";
const STALE_SPAWN_CALL_ID: &str = "spawn-call-stale";
const STALE_PATCH_CALL_ID: &str = "patch-call-stale";
const SET_REVIEW_GATE_CALL_ID: &str = "set-review-gate-call";
const AMEND_TASK_CALL_ID: &str = "amend-task-call";
const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";
const MULTI_AGENT_V2_NAMESPACE: &str = "agents";
const TURN_0_FORK_PROMPT: &str = "seed fork context";
const TURN_1_PROMPT: &str = "spawn a child and continue";
const TURN_2_NO_WAIT_PROMPT: &str = "follow up without wait";
const CHILD_PROMPT: &str = "child: do work";
const DEFAULT_SUBAGENT_MODEL: &str = "gpt-5.6-sol";
const DEFAULT_SUBAGENT_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::High;
const INHERITED_MODEL: &str = "gpt-5.2";
const INHERITED_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::Medium;
const REQUESTED_MODEL: &str = "gpt-5.4";
const REQUESTED_MODEL_WITH_DEFAULT_REASONING: &str = "gpt-5.6-sol";
const REQUESTED_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::Low;
const ROLE_MODEL: &str = "gpt-5.4";
const ROLE_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::High;
const SUBAGENT_START_CONTEXT: &str = "subagent start context reaches child";
const SUBAGENT_STOP_CONTINUATION: &str = "continue only the child";
const INTERNAL_SUBAGENT_PROMPT: &str = "internal subagent: review";
const TASK_CAPSULE_OPEN_TAG: &str = "<task_capsule_v1>";
const TASK_CAPSULE_CLOSE_TAG: &str = "</task_capsule_v1>";

fn body_contains(req: &wiremock::Request, text: &str) -> bool {
    decoded_body(req)
        .and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn prompt_cache_key(req: &wiremock::Request) -> Option<String> {
    decoded_body(req)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| {
            body.get("prompt_cache_key")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn request_has_input_type(req: &wiremock::Request, ty: &str) -> bool {
    decoded_body(req)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some(ty))
        })
}

fn request_has_call_output(req: &wiremock::Request, call_id: &str) -> bool {
    decoded_body(req)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
}

fn function_call_output_json(req: &wiremock::Request, call_id: &str) -> Option<Value> {
    decoded_body(req)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .and_then(|items| {
            items.iter().find_map(|item| {
                (item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id))
                .then(|| {
                    item.get("output")
                        .and_then(Value::as_str)
                        .and_then(|output| serde_json::from_str::<Value>(output).ok())
                })
                .flatten()
            })
        })
}

fn request_call_output_has_available_receipt(req: &wiremock::Request, call_id: &str) -> bool {
    available_receipt_assignment_id(req, call_id).is_some()
}

fn available_receipt_assignment_id(req: &wiremock::Request, call_id: &str) -> Option<String> {
    decoded_body(req)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .and_then(|items| {
            items.iter().find_map(|item| {
                if item.get("type").and_then(Value::as_str) != Some("function_call_output")
                    || item.get("call_id").and_then(Value::as_str) != Some(call_id)
                {
                    return None;
                }
                item.get("output")
                    .and_then(Value::as_str)
                    .and_then(|output| serde_json::from_str::<Value>(output).ok())
                    .and_then(|output| {
                        output
                            .get("typed_deltas")
                            .and_then(Value::as_array)
                            .cloned()
                    })
                    .and_then(|deltas| {
                        deltas.iter().find_map(|delta| {
                            let receipt = delta.get("receipt")?;
                            (receipt.get("available").and_then(Value::as_bool) == Some(true))
                                .then(|| {
                                    receipt
                                        .get("assignment_id")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned)
                                })
                                .flatten()
                        })
                    })
            })
        })
}

struct WaiveAvailableReceiptReviewGate;

impl Respond for WaiveAvailableReceiptReviewGate {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let assignment_id = available_receipt_assignment_id(request, WAIT_FOR_RECEIPT_CALL_ID)
            .expect("matched wait output must contain an available durable receipt");
        let arguments = serde_json::to_string(&json!({
            "assignment_id": assignment_id,
            "gate": "review",
            "reason": "fixture explicitly accepts the read-only child result"
        }))
        .expect("gate waiver arguments must serialize");
        sse_response(sse(vec![
            ev_response_created("resp-parent-waive-review"),
            ev_function_call_with_namespace(
                WAIVE_REVIEW_GATE_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "waive_agent_gate",
                &arguments,
            ),
            ev_completed("resp-parent-waive-review"),
        ]))
    }
}

struct WaiveReturnedReviewVerificationGate;

impl Respond for WaiveReturnedReviewVerificationGate {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let review_gate = function_call_output_json(request, WAIVE_REVIEW_GATE_CALL_ID)
            .and_then(|output| output.get("gate").cloned())
            .expect("matched review waiver output must contain a gate");
        assert_eq!(
            review_gate.get("kind").and_then(Value::as_str),
            Some("review")
        );
        assert_eq!(
            review_gate.get("status").and_then(Value::as_str),
            Some("waived")
        );
        let assignment_id = review_gate
            .get("assignment_id")
            .and_then(Value::as_str)
            .expect("waived review gate must identify its assignment");
        let arguments = serde_json::to_string(&json!({
            "assignment_id": assignment_id,
            "gate": "verification",
            "reason": "fixture explicitly accepts the read-only child without focused validation"
        }))
        .expect("verification gate waiver arguments must serialize");
        sse_response(sse(vec![
            ev_response_created("resp-parent-waive-verification"),
            ev_function_call_with_namespace(
                WAIVE_VERIFICATION_GATE_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "waive_agent_gate",
                &arguments,
            ),
            ev_completed("resp-parent-waive-verification"),
        ]))
    }
}

fn assert_waived_gate_tool_output(
    request: &ResponsesRequest,
    call_id: &str,
    assignment_id: &str,
    gate_kind: &str,
) -> Result<()> {
    let (output, success) = request
        .function_call_output_content_and_success(call_id)
        .ok_or_else(|| anyhow::anyhow!("missing {gate_kind} gate waiver tool output"))?;
    assert_eq!(
        success, None,
        "function-tool outputs carry their typed result as text"
    );
    let output = output.ok_or_else(|| anyhow::anyhow!("{gate_kind} gate waiver had no content"))?;
    let output: Value = serde_json::from_str(&output)?;
    assert_eq!(
        output
            .pointer("/gate/assignment_id")
            .and_then(Value::as_str),
        Some(assignment_id)
    );
    assert_eq!(
        output.pointer("/gate/kind").and_then(Value::as_str),
        Some(gate_kind)
    );
    assert_eq!(
        output.pointer("/gate/status").and_then(Value::as_str),
        Some("waived")
    );
    Ok(())
}

fn decoded_body(req: &wiremock::Request) -> Option<Vec<u8>> {
    let is_zstd = req
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&req.body)).ok()
    } else {
        Some(req.body.clone())
    }
}

fn has_subagent_notification(req: &ResponsesRequest) -> bool {
    req.message_input_texts("user")
        .iter()
        .any(|text| text.contains("<subagent_notification>"))
}

fn tool_parameter_description(tool: &Value, parameter_name: &str) -> Option<String> {
    tool.get("parameters")
        .and_then(|parameters| parameters.get("properties"))
        .and_then(|properties| properties.get(parameter_name))
        .and_then(|parameter| parameter.get("description"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn role_block(description: &str, role_name: &str) -> Option<String> {
    let role_header = format!("{role_name}: {{");
    let mut lines = description.lines().skip_while(|line| *line != role_header);
    let first_line = lines.next()?;
    let mut block = vec![first_line];
    for line in lines {
        if line.ends_with(": {") {
            break;
        }
        block.push(line);
    }
    Some(block.join("\n"))
}

fn write_home_skill(codex_home: &Path, dir: &str, name: &str, description: &str) -> Result<()> {
    let skill_dir = codex_home.join("skills").join(dir);
    fs::create_dir_all(&skill_dir)?;
    let contents = format!("---\nname: {name}\ndescription: {description}\n---\n\n# Body\n");
    fs::write(skill_dir.join("SKILL.md"), contents)?;
    Ok(())
}

fn write_subagent_lifecycle_hooks(
    home: &Path,
    stop_prompts: &[&str],
    subagent_stop_matcher: &str,
) -> Result<()> {
    let session_start_script_path = home.join("session_start_hook.py");
    let session_start_log_path = home.join("session_start_hook_log.jsonl");
    let session_start_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{session_start_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
"#,
        session_start_log_path = session_start_log_path.display(),
    );

    let start_script_path = home.join("subagent_start_hook.py");
    let start_log_path = home.join("subagent_start_hook_log.jsonl");
    let start_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{start_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
print(json.dumps({{"hookSpecificOutput": {{"hookEventName": "SubagentStart", "additionalContext": {SUBAGENT_START_CONTEXT:?}}}}}))
"#,
        start_log_path = start_log_path.display(),
    );

    let user_prompt_submit_script_path = home.join("user_prompt_submit_hook.py");
    let user_prompt_submit_log_path = home.join("user_prompt_submit_hook_log.jsonl");
    let user_prompt_submit_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{user_prompt_submit_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
"#,
        user_prompt_submit_log_path = user_prompt_submit_log_path.display(),
    );

    let subagent_stop_script_path = home.join("subagent_stop_hook.py");
    let subagent_stop_log_path = home.join("subagent_stop_hook_log.jsonl");
    let prompts_json = serde_json::to_string(stop_prompts)?;
    let subagent_stop_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{subagent_stop_log_path}")
block_prompts = {prompts_json}

payload = json.load(sys.stdin)
existing = []
if log_path.exists():
    existing = [line for line in log_path.read_text(encoding="utf-8").splitlines() if line.strip()]

with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")

invocation_index = len(existing)
if invocation_index < len(block_prompts):
    print(json.dumps({{"decision": "block", "reason": block_prompts[invocation_index]}}))
else:
    print(json.dumps({{"systemMessage": f"subagent stop pass {{invocation_index + 1}} complete"}}))
"#,
        subagent_stop_log_path = subagent_stop_log_path.display(),
        prompts_json = prompts_json,
    );

    let stop_script_path = home.join("stop_hook.py");
    let stop_log_path = home.join("stop_hook_log.jsonl");
    let stop_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{stop_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
print(json.dumps({{"systemMessage": "root stop complete"}}))
"#,
        stop_log_path = stop_log_path.display(),
    );

    let hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "startup",
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", session_start_script_path.display()),
                }]
            }],
            "SubagentStart": [{
                "matcher": "worker",
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", start_script_path.display()),
                }]
            }],
            "UserPromptSubmit": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", user_prompt_submit_script_path.display()),
                }]
            }],
            "SubagentStop": [{
                "matcher": subagent_stop_matcher,
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", subagent_stop_script_path.display()),
                }]
            }],
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", stop_script_path.display()),
                }]
            }]
        }
    });

    fs::write(&session_start_script_path, session_start_script)?;
    fs::write(&start_script_path, start_script)?;
    fs::write(&user_prompt_submit_script_path, user_prompt_submit_script)?;
    fs::write(&subagent_stop_script_path, subagent_stop_script)?;
    fs::write(&stop_script_path, stop_script)?;
    fs::write(home.join("hooks.json"), hooks.to_string())?;
    Ok(())
}

fn read_hook_log(home: &Path, filename: &str) -> Result<Vec<serde_json::Value>> {
    let path = home.join(filename);
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

async fn wait_for_hook_log(
    home: &Path,
    filename: &str,
    expected_len: usize,
) -> Result<Vec<serde_json::Value>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let inputs = read_hook_log(home, filename)?;
        if inputs.len() >= expected_len {
            return Ok(inputs);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "expected at least {expected_len} entries in {filename}, got {}",
                inputs.len()
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_spawned_thread_id(test: &TestCodex) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let ids = test.thread_manager.list_thread_ids().await;
        if let Some(spawned_id) = ids
            .iter()
            .find(|id| **id != test.session_configured.thread_id)
        {
            return Ok(spawned_id.to_string());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for spawned thread id");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_requests(
    mock: &core_test_support::responses::ResponseMock,
) -> Result<Vec<ResponsesRequest>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let requests = mock.requests();
        if !requests.is_empty() {
            return Ok(requests);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("expected at least 1 request, got {}", requests.len());
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn collect_events_through_turn_complete(thread: &CodexThread) -> Result<Vec<EventMsg>> {
    let mut events = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(10), thread.next_event())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for child TurnComplete"))??;
        let terminal = matches!(&event.msg, EventMsg::TurnComplete(_));
        events.push(event.msg);
        if terminal {
            return Ok(events);
        }
    }
}

fn raw_tool_output<'a>(events: &'a [EventMsg], call_id: &str) -> Option<&'a str> {
    events.iter().find_map(|event| {
        let EventMsg::RawResponseItem(raw) = event else {
            return None;
        };
        match &raw.item {
            ResponseItem::FunctionCallOutput {
                call_id: output_call_id,
                output,
                ..
            }
            | ResponseItem::CustomToolCallOutput {
                call_id: output_call_id,
                output,
                ..
            } if output_call_id == call_id => output.text_content(),
            _ => None,
        }
    })
}

#[derive(Clone, Copy)]
enum TypedChildDelayedCompletion {
    Success,
    StaleReceipt(&'static str),
    TerminalError,
}

async fn setup_typed_v2_child_with_delayed_completion(
    server: &MockServer,
    delay: Duration,
    delayed_completion: TypedChildDelayedCompletion,
) -> Result<(TestCodex, Arc<CodexThread>, String)> {
    let spawn_args = serde_json::to_string(&json!({
        "task_name": "explorer",
        "agent_type": "explorer",
        "assignment": {
            "objective": CHILD_PROMPT,
            "acceptance_criteria": [{
                "id": "legacy-message-result",
                "text": "Return a concrete result for the requested task to the parent agent."
            }],
            "read_scope": [{"path": ".", "recursive": true}],
            "write_scope": [],
            "stop_condition": "Stop after reporting the requested result to the parent agent.",
            "required_evidence": []
        }
    }))?;
    mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let receipt_args = serde_json::to_string(&json!({
        "status": "completed",
        "summary": "child done",
        "criterion_results": [{
            "criterion_id": "legacy-message-result",
            "status": "passed",
            "evidence": "child result completed"
        }],
        "declared_changes": [],
        "validation_call_ids": [],
        "blockers": [],
        "risks": [],
        "next_action": null
    }))?;
    mount_sse_once_match(
        server,
        |req: &wiremock::Request| {
            body_contains(req, TASK_CAPSULE_OPEN_TAG) && body_contains(req, CHILD_PROMPT)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_function_call_with_namespace(
                RECEIPT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "submit_agent_receipt",
                &receipt_args,
            ),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    let delayed_child_events = match delayed_completion {
        TypedChildDelayedCompletion::StaleReceipt(call_id) => vec![
            ev_response_created("resp-child-2"),
            ev_function_call_with_namespace(
                STALE_GET_TASK_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "get_agent_task",
                "{}",
            ),
            ev_function_call_with_namespace(
                STALE_SET_GATE_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "set_agent_gate",
                "{}",
            ),
            ev_function_call_with_namespace(
                STALE_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                r#"{"message":"stale child spawn","task_name":"stale-descendant"}"#,
            ),
            ev_apply_patch_custom_tool_call(
                STALE_PATCH_CALL_ID,
                "*** Begin Patch\n*** Add File: stale-child-mutation.txt\n+must not exist\n*** End Patch",
            ),
            ev_function_call_with_namespace(
                call_id,
                MULTI_AGENT_V2_NAMESPACE,
                "submit_agent_receipt",
                &receipt_args,
            ),
            ev_completed("resp-child-2"),
        ],
        TypedChildDelayedCompletion::Success => vec![
            ev_response_created("resp-child-2"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-2"),
        ],
        TypedChildDelayedCompletion::TerminalError => vec![
            ev_response_created("resp-child-2"),
            ev_assistant_message(
                "msg-child-failed-draft",
                "child drafted output before failure",
            ),
        ],
    };
    let child_completion_request = mount_response_once_match(
        server,
        |req: &wiremock::Request| request_has_call_output(req, RECEIPT_CALL_ID),
        sse_response(sse(delayed_child_events)).set_delay(delay),
    )
    .await;
    mount_sse_once_match(
        server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID) && !request_has_input_type(req, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_function_call_with_namespace(
                WAIT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;
    mount_sse_once_match(
        server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_1_PROMPT) && request_has_call_output(req, WAIT_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-parent-3"),
            ev_function_call_with_namespace(
                WAIT_FOR_RECEIPT_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("resp-parent-3"),
        ]),
    )
    .await;
    let gate_waiver_completion_requests = if matches!(
        delayed_completion,
        TypedChildDelayedCompletion::Success | TypedChildDelayedCompletion::TerminalError
    ) {
        Mock::given(method("POST"))
            .and(path_regex(".*/responses$"))
            .and(|req: &wiremock::Request| {
                body_contains(req, TURN_1_PROMPT)
                    && request_call_output_has_available_receipt(req, WAIT_FOR_RECEIPT_CALL_ID)
            })
            .respond_with(WaiveAvailableReceiptReviewGate)
            .up_to_n_times(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(".*/responses$"))
            .and(|req: &wiremock::Request| request_has_call_output(req, WAIVE_REVIEW_GATE_CALL_ID))
            .respond_with(WaiveReturnedReviewVerificationGate)
            .up_to_n_times(1)
            .mount(server)
            .await;
        let verification_waiver_request = mount_sse_once_match(
            server,
            |req: &wiremock::Request| request_has_call_output(req, WAIVE_VERIFICATION_GATE_CALL_ID),
            sse(vec![
                ev_response_created("resp-parent-4"),
                ev_assistant_message("msg-parent-4", "parent done"),
                ev_completed("resp-parent-4"),
            ]),
        )
        .await;
        Some(verification_waiver_request)
    } else {
        mount_sse_once_match(
            server,
            |req: &wiremock::Request| {
                body_contains(req, TURN_1_PROMPT)
                    && request_call_output_has_available_receipt(req, WAIT_FOR_RECEIPT_CALL_ID)
            },
            sse(vec![
                ev_response_created("resp-parent-4"),
                ev_assistant_message("msg-parent-4", "parent done"),
                ev_completed("resp-parent-4"),
            ]),
        )
        .await;
        None
    };

    let test = test_codex()
        .with_model("koffing")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        })
        .with_workspace_setup(|cwd, _fs| async move {
            let init_output = tokio::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(cwd.as_path())
                .output()
                .await?;
            if !init_output.status.success() {
                anyhow::bail!(
                    "initialize typed-child test repository: {}",
                    String::from_utf8_lossy(&init_output.stderr)
                );
            }
            Ok(())
        })
        .build(server)
        .await?;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: TURN_1_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;
    let child_thread_id = ThreadId::from_string(&wait_for_spawned_thread_id(&test).await?)?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    if matches!(
        delayed_completion,
        TypedChildDelayedCompletion::StaleReceipt(_)
    ) {
        child_thread.request_raw_response_items();
    }
    let receipt_deadline = Instant::now() + Duration::from_secs(10);
    let receipt_request = loop {
        if let Some(request) = child_completion_request
            .requests()
            .into_iter()
            .find(|request| {
                request
                    .body_json()
                    .get("input")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                                && item.get("call_id").and_then(Value::as_str)
                                    == Some(RECEIPT_CALL_ID)
                        })
                    })
            })
        {
            break request;
        }
        if Instant::now() >= receipt_deadline {
            let received = server.received_requests().await.unwrap_or_default();
            let decoded_inputs = received
                .iter()
                .map(|request| {
                    decoded_body(request)
                        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
                })
                .collect::<Vec<_>>();
            let function_call_outputs = decoded_inputs
                .iter()
                .enumerate()
                .flat_map(|(request_index, items)| {
                    items.iter().flatten().filter_map(move |item| {
                        (item.get("type").and_then(Value::as_str) == Some("function_call_output"))
                            .then(|| {
                                json!({
                                    "request_index": request_index,
                                    "payload": item,
                                })
                            })
                    })
                })
                .collect::<Vec<_>>();
            let spawn_call_outputs = function_call_outputs
                .iter()
                .filter(|record| {
                    record
                        .get("payload")
                        .and_then(|payload| payload.get("call_id"))
                        .and_then(Value::as_str)
                        == Some(SPAWN_CALL_ID)
                })
                .collect::<Vec<_>>();
            anyhow::bail!(
                "missing durable receipt request; received_request_count={}; \
                 spawn_call_outputs={}; function_call_outputs={}; decoded_inputs={}",
                received.len(),
                serde_json::to_string(&spawn_call_outputs)?,
                serde_json::to_string(&function_call_outputs)?,
                serde_json::to_string(&decoded_inputs)?,
            );
        }
        sleep(Duration::from_millis(10)).await;
    };
    let (receipt_output, _) = receipt_request
        .function_call_output_content_and_success(RECEIPT_CALL_ID)
        .ok_or_else(|| anyhow::anyhow!("missing durable receipt tool output"))?;
    let receipt_output = receipt_output
        .ok_or_else(|| anyhow::anyhow!("durable receipt tool output had no content"))?;
    assert!(receipt_output.contains("completed"), "{receipt_output}");
    assert!(receipt_output.contains("child done"), "{receipt_output}");
    let receipt_json = serde_json::Deserializer::from_str(&receipt_output)
        .into_iter::<Value>()
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .find(|record| record.pointer("/receipt/assignment_id").is_some())
        .ok_or_else(|| anyhow::anyhow!("durable receipt output omitted receipt record"))?;
    let assignment_id = receipt_json
        .get("receipt")
        .and_then(|receipt| receipt.get("assignment_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("durable receipt output omitted assignment_id"))?
        .to_string();
    if let Some(verification_waiver_request) = gate_waiver_completion_requests {
        let verification_waiver_requests = wait_for_requests(&verification_waiver_request).await?;
        assert_waived_gate_tool_output(
            &verification_waiver_requests[0],
            WAIVE_VERIFICATION_GATE_CALL_ID,
            &assignment_id,
            "verification",
        )?;
    }
    Ok((test, child_thread, assignment_id))
}

async fn assert_resumed_root_rejects_failed_terminal_delivery(
    test: &TestCodex,
    server: &MockServer,
    parent_rollout_path: std::path::PathBuf,
    assignment_id: &str,
    prompt: &'static str,
    durable_summary: &str,
) -> Result<()> {
    let get_task_args = serde_json::to_string(&json!({
        "assignment_id": assignment_id,
    }))?;
    mount_sse_once_match(
        server,
        move |req: &wiremock::Request| body_contains(req, prompt),
        sse(vec![
            ev_response_created("resp-root-resumed"),
            ev_function_call_with_namespace(
                RESUMED_GET_TASK_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "get_agent_task",
                &get_task_args,
            ),
            ev_completed("resp-root-resumed"),
        ]),
    )
    .await;
    let inspected_task = mount_sse_once_match(
        server,
        |req: &wiremock::Request| request_has_call_output(req, RESUMED_GET_TASK_CALL_ID),
        sse(vec![
            ev_response_created("resp-root-resumed-after-inspection"),
            ev_assistant_message("msg-root-resumed", "root attempted certification"),
            ev_completed("resp-root-resumed-after-inspection"),
        ]),
    )
    .await;
    let resumed = test
        .thread_manager
        .resume_thread_from_rollout(
            test.config.clone(),
            parent_rollout_path,
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?
        .thread;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    resumed
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;
    let inspected_task_requests = wait_for_requests(&inspected_task).await?;
    let inspected_task_output = inspected_task_requests[0]
        .function_call_output_text(RESUMED_GET_TASK_CALL_ID)
        .ok_or_else(|| anyhow::anyhow!("resumed root did not receive get_agent_task output"))?;
    assert!(
        inspected_task_output.contains(assignment_id)
            && inspected_task_output.contains(durable_summary),
        "durable task inspection omitted the failed terminal outcome: {inspected_task_output}"
    );
    let events = collect_events_through_turn_complete(&resumed).await?;
    assert!(
        !events.iter().any(|event| {
            matches!(event, EventMsg::AgentMessage(message) if message.message == "root attempted certification")
        }),
        "resumed root published success using an undelivered child receipt: {events:#?}"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .expect("resumed root TurnComplete");
    let error = completion
        .error
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("resumed root unexpectedly certified completion"))?;
    assert!(
        error.message.contains("current typed-task attempt")
            && error.message.contains("has failed terminal delivery"),
        "resumed root rejected for an unexpected reason: {}",
        error.message
    );
    resumed.shutdown_and_wait().await?;
    Ok(())
}

async fn setup_turn_one_with_spawned_child(
    server: &MockServer,
    child_response_delay: Option<Duration>,
) -> Result<(TestCodex, String)> {
    let (test, spawned_id, _child_request_log) = setup_turn_one_with_custom_spawned_child(
        server,
        json!({
            "message": CHILD_PROMPT,
        }),
        child_response_delay,
        /*wait_for_parent_notification*/ true,
        |builder| builder,
    )
    .await?;
    Ok((test, spawned_id))
}

async fn setup_turn_one_with_custom_spawned_child(
    server: &MockServer,
    spawn_args: serde_json::Value,
    child_response_delay: Option<Duration>,
    wait_for_parent_notification: bool,
    configure_test: impl FnOnce(
        core_test_support::test_codex::TestCodexBuilder,
    ) -> core_test_support::test_codex::TestCodexBuilder,
) -> Result<(
    TestCodex,
    String,
    core_test_support::responses::ResponseMock,
)> {
    let spawn_args = serde_json::to_string(&spawn_args)?;

    mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_sse = sse(vec![
        ev_response_created("resp-child-1"),
        ev_assistant_message("msg-child-1", "child done"),
        ev_completed("resp-child-1"),
    ]);
    let child_request_log = if let Some(delay) = child_response_delay {
        mount_response_once_match(
            server,
            |req: &wiremock::Request| {
                body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
            },
            sse_response(child_sse).set_delay(delay),
        )
        .await
    } else {
        mount_sse_once_match(
            server,
            |req: &wiremock::Request| {
                body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
            },
            child_sse,
        )
        .await
    };

    let _turn1_followup = mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = configure_test(test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config.model = Some(INHERITED_MODEL.to_string());
        config.model_reasoning_effort = Some(INHERITED_REASONING_EFFORT);
    }));
    let test = builder.build(server).await?;
    test.submit_turn(TURN_1_PROMPT).await?;
    if child_response_delay.is_none() && wait_for_parent_notification {
        let _ = wait_for_requests(&child_request_log).await?;
        let rollout_path = test
            .codex
            .rollout_path()
            .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let has_notification = tokio::fs::read_to_string(&rollout_path)
                .await
                .is_ok_and(|rollout| rollout.contains("<subagent_notification>"));
            if has_notification {
                break;
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for parent rollout to include subagent notification"
                );
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
    let spawned_id = wait_for_spawned_thread_id(&test).await?;

    Ok((test, spawned_id, child_request_log))
}

async fn spawn_child_and_capture_snapshot(
    server: &MockServer,
    spawn_args: serde_json::Value,
    configure_test: impl FnOnce(
        core_test_support::test_codex::TestCodexBuilder,
    ) -> core_test_support::test_codex::TestCodexBuilder,
) -> Result<ThreadConfigSnapshot> {
    let (test, spawned_id, _child_request_log) = setup_turn_one_with_custom_spawned_child(
        server,
        spawn_args,
        /*child_response_delay*/ None,
        /*wait_for_parent_notification*/ false,
        configure_test,
    )
    .await?;
    let thread_id = ThreadId::from_string(&spawned_id)?;
    Ok(test
        .thread_manager
        .get_thread(thread_id)
        .await?
        .config_snapshot()
        .await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_start_replaces_session_start_and_injects_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "child",
        "agent_type": "worker",
    }))?;

    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT)
                && body_contains(req, SUBAGENT_START_CONTEXT)
                && !body_contains(req, "<subagent_notification>")
                && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_pre_build_hook(|home| {
            write_subagent_lifecycle_hooks(home, /*stop_prompts*/ &[], "worker")
                .expect("failed to write subagent hook fixture");
        })
        .with_config(|config| {
            trust_discovered_hooks(config);
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = wait_for_requests(&child_request_log).await?;

    let start_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "subagent_start_hook_log.jsonl",
        /*expected_len*/ 1,
    )
    .await?;
    assert_eq!(start_inputs.len(), 1);
    assert_eq!(start_inputs[0]["agent_type"].as_str(), Some("worker"));
    let spawned_id = wait_for_spawned_thread_id(&test).await?;
    assert_eq!(
        start_inputs[0]["agent_id"].as_str(),
        Some(spawned_id.as_str())
    );

    let user_prompt_submit_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "user_prompt_submit_hook_log.jsonl",
        /*expected_len*/ 2,
    )
    .await?;
    let parent_prompt_input = user_prompt_submit_inputs
        .iter()
        .find(|input| input["prompt"].as_str() == Some(TURN_1_PROMPT))
        .expect("parent prompt submit hook input should be logged");
    assert_eq!(parent_prompt_input.get("agent_id"), None);
    assert_eq!(parent_prompt_input.get("agent_type"), None);

    let child_prompt_input = user_prompt_submit_inputs
        .iter()
        .find(|input| input["prompt"].as_str() == Some(CHILD_PROMPT))
        .expect("child prompt submit hook input should be logged");
    assert_eq!(
        child_prompt_input["agent_id"].as_str(),
        Some(spawned_id.as_str())
    );
    assert_eq!(child_prompt_input["agent_type"].as_str(), Some("worker"));

    let session_start_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "session_start_hook_log.jsonl",
        /*expected_len*/ 1,
    )
    .await?;
    assert_eq!(session_start_inputs.len(), 1);
    assert_eq!(session_start_inputs[0]["source"].as_str(), Some("startup"));
    assert_ne!(
        session_start_inputs[0]["session_id"].as_str(),
        Some(spawned_id.as_str())
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_stop_replaces_stop_and_skips_internal_subagents() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "child",
        "agent_type": "worker",
    }))?;

    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let first_child_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done first"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    let second_child_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SUBAGENT_STOP_CONTINUATION) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-2"),
            ev_assistant_message("msg-child-2", "child done final"),
            ev_completed("resp-child-2"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;
    let internal_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, INTERNAL_SUBAGENT_PROMPT),
        sse(vec![
            ev_response_created("resp-internal-1"),
            ev_assistant_message("msg-internal-1", "internal subagent done"),
            ev_completed("resp-internal-1"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_pre_build_hook(|home| {
            write_subagent_lifecycle_hooks(
                home,
                /*stop_prompts*/ &[SUBAGENT_STOP_CONTINUATION],
                "",
            )
            .expect("failed to write subagent hook fixture");
        })
        .with_config(|config| {
            trust_discovered_hooks(config);
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = wait_for_requests(&first_child_request).await?;
    let _ = wait_for_requests(&second_child_request).await?;

    let subagent_stop_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "subagent_stop_hook_log.jsonl",
        /*expected_len*/ 2,
    )
    .await?;
    assert_eq!(subagent_stop_inputs.len(), 2);
    assert_eq!(
        subagent_stop_inputs
            .iter()
            .map(|input| input["stop_hook_active"].as_bool())
            .collect::<Vec<_>>(),
        vec![Some(false), Some(true)]
    );
    assert_eq!(
        subagent_stop_inputs[0]["agent_type"].as_str(),
        Some("worker")
    );
    let parent_transcript_path = subagent_stop_inputs[0]["transcript_path"]
        .as_str()
        .expect("SubagentStop should include parent transcript_path");
    let agent_transcript_path = subagent_stop_inputs[0]["agent_transcript_path"]
        .as_str()
        .expect("SubagentStop should include agent_transcript_path");
    assert_ne!(parent_transcript_path, agent_transcript_path);
    assert_eq!(
        subagent_stop_inputs[1]["transcript_path"].as_str(),
        Some(parent_transcript_path)
    );
    assert_eq!(
        subagent_stop_inputs[1]["agent_transcript_path"].as_str(),
        Some(agent_transcript_path)
    );
    assert_eq!(
        subagent_stop_inputs[0]["last_assistant_message"].as_str(),
        Some("child done first")
    );

    let stop_inputs = read_hook_log(test.codex_home_path(), "stop_hook_log.jsonl")?;
    assert!(
        stop_inputs
            .iter()
            .all(|input| input["last_assistant_message"].as_str() != Some("child done first")),
        "child completion should not invoke the normal Stop hook"
    );
    let stop_input_count = stop_inputs.len();

    // This matcher would catch the old synthetic "review" SubagentStop target
    // because the SubagentStop hook above intentionally matches all agent types.
    let internal_thread = test
        .thread_manager
        .start_thread_with_options(StartThreadOptions {
            config: test.config.clone(),
            allow_provider_model_fallback: false,
            initial_history: InitialHistory::New,
            history_mode: None,
            session_source: Some(SessionSource::SubAgent(SubAgentSource::Review)),
            thread_source: None,
            dynamic_tools: Vec::new(),
            metrics_service_name: None,
            parent_trace: None,
            environments: Vec::new(),
            thread_extension_init: Default::default(),
            supports_openai_form_elicitation: false,
        })
        .await?;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    internal_thread
        .thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: INTERNAL_SUBAGENT_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                model: Some(internal_thread.session_configured.model.clone()),
                ..Default::default()
            },
        })
        .await?;
    let turn_id = wait_for_event_match(internal_thread.thread.as_ref(), |event| match event {
        EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
        _ => None,
    })
    .await;
    wait_for_event_match(internal_thread.thread.as_ref(), |event| match event {
        EventMsg::TurnComplete(event) if event.turn_id == turn_id => Some(()),
        _ => None,
    })
    .await;
    let requests = wait_for_requests(&internal_request).await?;
    assert_eq!(requests.len(), 1);

    let subagent_stop_inputs_after_internal =
        read_hook_log(test.codex_home_path(), "subagent_stop_hook_log.jsonl")?;
    assert_eq!(subagent_stop_inputs_after_internal, subagent_stop_inputs);

    let stop_inputs_after_internal = read_hook_log(test.codex_home_path(), "stop_hook_log.jsonl")?;
    assert_eq!(stop_inputs_after_internal.len(), stop_input_count);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_notification_is_included_without_wait() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, _spawned_id) =
        setup_turn_one_with_spawned_child(&server, /*child_response_delay*/ None).await?;

    let turn2 = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_2_NO_WAIT_PROMPT),
        sse(vec![
            ev_response_created("resp-turn2-1"),
            ev_assistant_message("msg-turn2-1", "no wait path"),
            ev_completed("resp-turn2-1"),
        ]),
    )
    .await;
    test.submit_turn(TURN_2_NO_WAIT_PROMPT).await?;

    let turn2_requests = wait_for_requests(&turn2).await?;
    assert!(turn2_requests.iter().any(has_subagent_notification));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_receives_forked_parent_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let seed_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_0_FORK_PROMPT),
        sse(vec![
            ev_response_created("resp-seed-1"),
            ev_assistant_message("msg-seed-1", "seeded"),
            ev_completed("resp-seed-1"),
        ]),
    )
    .await;

    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "fork_context": true,
    }))?;
    let spawn_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let parent_prompt_cache_key = Arc::new(Mutex::new(None::<String>));
    let child_parent_prompt_cache_key = Arc::clone(&parent_prompt_cache_key);
    let _child_request_log = mount_sse_once_match(
        &server,
        move |req: &wiremock::Request| {
            let parent_key = child_parent_prompt_cache_key
                .lock()
                .expect("parent prompt cache key lock poisoned")
                .clone();
            req.url.path() == "/v1/responses"
                && parent_key.is_some()
                && prompt_cache_key(req) != parent_key
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
    });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_0_FORK_PROMPT).await?;
    let seed_request = seed_turn.single_request();
    let seed_prompt_cache_key = seed_request.body_json()["prompt_cache_key"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("seed request should include prompt_cache_key"))?;
    *parent_prompt_cache_key
        .lock()
        .expect("parent prompt cache key lock poisoned") = Some(seed_prompt_cache_key.clone());

    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = spawn_turn.single_request();

    let deadline = Instant::now() + Duration::from_secs(2);
    let child_request = loop {
        if let Some(request) = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|request| {
                request.url.path() == "/v1/responses"
                    && prompt_cache_key(request)
                        .is_some_and(|request_key| request_key != seed_prompt_cache_key)
            })
        {
            break request;
        }
        if Instant::now() >= deadline {
            let observed = server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|request| {
                    (
                        request.url.path().to_string(),
                        prompt_cache_key(&request),
                        body_contains(&request, TURN_0_FORK_PROMPT),
                        body_contains(&request, TURN_1_PROMPT),
                        body_contains(&request, SPAWN_CALL_ID),
                    )
                })
                .collect::<Vec<_>>();
            anyhow::bail!("timed out waiting for forked child request; observed={observed:?}");
        }
        sleep(Duration::from_millis(10)).await;
    };
    assert!(body_contains(&child_request, TURN_0_FORK_PROMPT));
    assert!(!body_contains(&child_request, SPAWN_CALL_ID));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_uses_built_in_model_and_reasoning_defaults() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
        }),
        |builder| builder,
    )
    .await?;

    assert_eq!(child_snapshot.model, DEFAULT_SUBAGENT_MODEL);
    assert_eq!(
        child_snapshot.reasoning_effort,
        Some(DEFAULT_SUBAGENT_REASONING_EFFORT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "model": REQUESTED_MODEL,
            "reasoning_effort": REQUESTED_REASONING_EFFORT,
        }),
        |builder| builder,
    )
    .await?;

    assert_eq!(child_snapshot.model, REQUESTED_MODEL);
    assert_eq!(
        child_snapshot.reasoning_effort,
        Some(REQUESTED_REASONING_EFFORT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_model_override_keeps_built_in_reasoning_default() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "model": REQUESTED_MODEL_WITH_DEFAULT_REASONING,
        }),
        |builder| builder,
    )
    .await?;

    assert_eq!(child_snapshot.model, REQUESTED_MODEL_WITH_DEFAULT_REASONING);
    assert_eq!(
        child_snapshot.reasoning_effort,
        Some(DEFAULT_SUBAGENT_REASONING_EFFORT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_instruction_only_role_keeps_built_in_model_defaults() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "agent_type": "custom",
        }),
        |builder| {
            builder.with_config(|config| {
                let role_path = config.codex_home.join("instruction-only-role.toml");
                std::fs::write(&role_path, "developer_instructions = \"Stay focused\"\n")
                    .expect("write instruction-only role config");
                config.agent_roles.insert(
                    "custom".to_string(),
                    AgentRoleConfig {
                        description: Some("Instruction-only role".to_string()),
                        config_file: Some(role_path.to_path_buf()),
                        nickname_candidates: None,
                    },
                );
            })
        },
    )
    .await?;

    assert_eq!(child_snapshot.model, DEFAULT_SUBAGENT_MODEL);
    assert_eq!(
        child_snapshot.reasoning_effort,
        Some(DEFAULT_SUBAGENT_REASONING_EFFORT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_multi_agent_v2_child_inherits_developer_context_without_parent_history()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let seed_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_0_FORK_PROMPT) && !request_has_input_type(req, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-seed-1"),
            ev_assistant_message("msg-seed-1", "seeded"),
            ev_completed("resp-seed-1"),
        ]),
    )
    .await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_1_PROMPT) && !request_has_input_type(req, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID) && !request_has_input_type(req, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config.developer_instructions = Some("Parent developer instructions.".to_string());
    });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_0_FORK_PROMPT).await?;
    let _ = seed_turn.single_request();
    test.submit_turn(TURN_1_PROMPT).await?;

    let deadline = Instant::now() + Duration::from_secs(2);
    let child_request = loop {
        if let Some(request) = child_request_log.requests().into_iter().find(|request| {
            request.body_contains_text(TASK_CAPSULE_OPEN_TAG)
                && request.body_contains_text(CHILD_PROMPT)
        }) {
            break request;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for child task capsule request");
        }
        sleep(Duration::from_millis(10)).await;
    };
    assert!(child_request.body_contains_text("Parent developer instructions."));
    assert!(child_request.body_contains_text(CHILD_PROMPT));
    assert!(!child_request.body_contains_text(TURN_0_FORK_PROMPT));

    Ok(())
}

#[tokio::test]
async fn legacy_multi_agent_v2_spawn_sends_task_capsule_to_child() -> Result<()> {
    let server = start_mock_server().await;
    let child_objective = "durable child objective";
    let spawn_args = serde_json::to_string(&json!({
        "message": child_objective,
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TASK_CAPSULE_OPEN_TAG) && body_contains(req, child_objective)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID) && !request_has_input_type(req, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
    });
    let test = builder.build(&server).await?;
    let root_thread_id = test.session_configured.thread_id;

    test.submit_turn(TURN_1_PROMPT).await?;

    // The response mock records candidate requests before its request matcher runs, so wait for
    // the child request instead of assuming the latest recorded request is already it.
    let deadline = Instant::now() + Duration::from_secs(2);
    let child_request = loop {
        if let Some(request) = child_request_log.requests().into_iter().find(|request| {
            request.body_contains_text(TASK_CAPSULE_OPEN_TAG)
                && request.body_contains_text(child_objective)
        }) {
            break request;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for child task capsule request");
        }
        sleep(Duration::from_millis(10)).await;
    };
    assert!(child_request.has_message_with_input_texts("user", |texts| {
        texts.iter().any(|text| {
            text.starts_with(TASK_CAPSULE_OPEN_TAG)
                && text.ends_with(TASK_CAPSULE_CLOSE_TAG)
                && text.contains(child_objective)
        })
    }));
    assert!(child_request.inputs_of_type("agent_message").is_empty());

    let child_thread_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| *thread_id != root_thread_id)
        .expect("child thread ID");
    let child_snapshot = test
        .thread_manager
        .get_thread(child_thread_id)
        .await?
        .config_snapshot()
        .await;
    assert_eq!(child_snapshot.model, DEFAULT_SUBAGENT_MODEL);
    assert_eq!(
        child_snapshot.reasoning_effort,
        Some(DEFAULT_SUBAGENT_REASONING_EFFORT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_v2_child_publishes_output_after_receipt_delivery() -> Result<()> {
    let server = start_mock_server().await;
    let (test, child_thread, _) = setup_typed_v2_child_with_delayed_completion(
        &server,
        Duration::from_secs(1),
        TypedChildDelayedCompletion::Success,
    )
    .await?;

    let events = collect_events_through_turn_complete(&child_thread).await?;
    let terminal_index = events
        .iter()
        .position(|event| matches!(event, EventMsg::TurnComplete(_)))
        .unwrap_or_else(|| panic!("child TurnComplete was not emitted: {events:#?}"));

    let EventMsg::TurnComplete(completion) = &events[terminal_index] else {
        unreachable!("terminal index must identify TurnComplete");
    };
    assert!(
        completion.error.is_none(),
        "successful receipt delivery produced a failed child terminal event: {events:#?}"
    );
    assert_eq!(
        child_thread.agent_status().await,
        AgentStatus::Completed(Some("child done".to_string())),
        "live agent status must agree with the durable receipt-backed terminal event: {events:#?}"
    );

    let output_index = events
        .iter()
        .position(|event| {
            matches!(event, EventMsg::AgentMessage(message) if message.message == "child done")
        })
        .unwrap_or_else(|| {
            panic!("successful receipt delivery did not publish child output: {events:#?}")
        });
    assert!(
        output_index < terminal_index,
        "child output must be published before the successful terminal event: {events:#?}"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("child done"),
        "successful child terminal event must expose the published output: {events:#?}"
    );

    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_2_NO_WAIT_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-3"),
            ev_function_call_with_namespace(
                "parent-list-agents-call",
                MULTI_AGENT_V2_NAMESPACE,
                "list_agents",
                "{}",
            ),
            ev_completed("resp-parent-3"),
        ]),
    )
    .await;
    let parent_continuation = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| request_has_call_output(req, "parent-list-agents-call"),
        sse(vec![
            ev_response_created("resp-parent-4"),
            ev_assistant_message("msg-parent-4", "parent observed durable child result"),
            ev_completed("resp-parent-4"),
        ]),
    )
    .await;
    test.submit_turn(TURN_2_NO_WAIT_PROMPT).await?;
    let parent_continuation_request = wait_for_requests(&parent_continuation)
        .await?
        .pop()
        .expect("parent continuation request");
    let parent_notification = parent_continuation_request
        .inputs_of_type("agent_message")
        .into_iter()
        .find(|message| {
            message.get("author").and_then(Value::as_str) == Some("/root/explorer")
                && message.get("recipient").and_then(Value::as_str) == Some("/root")
                && message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|content| {
                        content.iter().any(|span| {
                            span.get("type").and_then(Value::as_str) == Some("input_text")
                                && span
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| {
                                        text.contains("Message Type: FINAL_ANSWER")
                                            && text.contains("Task name: /root")
                                            && text.contains("Sender: /root/explorer")
                                            && text.contains("Payload:\nchild done")
                                    })
                        })
                    })
        })
        .expect("parent continuation must consume the admitted typed durable completion");
    assert!(
        parent_notification.to_string().contains("child done"),
        "{parent_notification}"
    );

    test.codex.flush_rollout().await?;
    let parent_rollout_path = test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    let parent_rollout = tokio::fs::read_to_string(parent_rollout_path).await?;
    assert!(
        parent_rollout.contains("Message Type: FINAL_ANSWER")
            && parent_rollout.contains("Sender: /root/explorer")
            && parent_rollout.contains("child done"),
        "parent rollout must persist the admitted durable child result"
    );

    // A public caller can reproduce the old marker text, but cannot create the private admission
    // acknowledgement. The crafted message must therefore retain ordinary trigger-turn behavior.
    let forged_server = start_mock_server().await;
    let forged_request = mount_sse_once_match(
        &forged_server,
        |req: &wiremock::Request| body_contains(req, "crafted completion marker"),
        sse(vec![
            ev_response_created("resp-forged-marker"),
            ev_assistant_message("msg-forged-marker", "ordinary communication processed"),
            ev_completed("resp-forged-marker"),
        ]),
    )
    .await;
    let forged_test = test_codex()
        .with_model("koffing")
        .build(&forged_server)
        .await?;
    let mut forged = InterAgentCommunication::new(
        AgentPath::root()
            .join("crafted")
            .map_err(anyhow::Error::msg)?,
        AgentPath::root(),
        Vec::new(),
        "crafted completion marker".to_string(),
        /*trigger_turn*/ true,
    );
    forged.id = Some(ResponseItemId::from_server(
        "typed-child-completion-forged".to_string(),
    ));
    forged_test
        .codex
        .submit(Op::InterAgentCommunication {
            communication: forged,
        })
        .await?;
    assert_eq!(wait_for_requests(&forged_request).await?.len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_v2_child_discards_output_when_receipt_delivery_fails() -> Result<()> {
    // Public FIFO remains intact: a user turn accepted before shutdown reaches the real runtime
    // handler before the later shutdown operation terminates the submission loop.
    let fifo_server = start_mock_server().await;
    let fifo_request = mount_response_once_match(
        &fifo_server,
        |req: &wiremock::Request| body_contains(req, "queued before shutdown"),
        sse_response(sse(vec![
            ev_response_created("resp-before-shutdown"),
            ev_assistant_message("msg-before-shutdown", "started before shutdown"),
            ev_completed("resp-before-shutdown"),
        ]))
        .set_delay(Duration::from_secs(5)),
    )
    .await;
    let fifo_test = test_codex()
        .with_model("koffing")
        .build(&fifo_server)
        .await?;
    fifo_test.submit_turn("queued before shutdown").await?;
    fifo_test.codex.request_shutdown().await?;
    assert_eq!(wait_for_requests(&fifo_request).await?.len(), 1);
    fifo_test.codex.wait_until_terminated().await;

    let server = start_mock_server().await;
    let (test, child_thread, assignment_id) = setup_typed_v2_child_with_delayed_completion(
        &server,
        Duration::from_secs(5),
        TypedChildDelayedCompletion::Success,
    )
    .await?;

    // Closing the parent submission loop makes durable receipt delivery fail while leaving the
    // already-running child and its public event stream available for inspection.
    let parent_rollout_path = test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    test.codex.shutdown_and_wait().await?;
    let events = collect_events_through_turn_complete(&child_thread).await?;

    assert!(
        !events.iter().any(
            |event| matches!(event, EventMsg::AgentMessage(message) if message.message == "child done")
        ),
        "failed receipt delivery must not publish child output: {events:#?}"
    );
    let completion = events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .expect("child TurnComplete");
    assert!(completion.last_agent_message.is_none(), "{completion:#?}");
    assert!(completion.surfaced_result.is_none(), "{completion:#?}");
    assert_eq!(
        completion
            .error
            .as_ref()
            .map(|error| error.message.as_str()),
        Some("typed child completion could not seal and deliver its durable evidence receipt")
    );
    assert!(
        matches!(
            child_thread.agent_status().await,
            AgentStatus::Errored(message)
                if message == "typed child completion could not seal and deliver its durable evidence receipt"
        ),
        "live child status must agree with the failed terminal delivery"
    );

    child_thread.flush_rollout().await?;
    let child_rollout_path = child_thread
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected child rollout path"))?;
    let child_rollout = tokio::fs::read_to_string(&child_rollout_path).await?;
    assert!(
        !child_rollout.contains("msg-child-1"),
        "discarded child response leaked into persisted rollout: {child_rollout}"
    );
    child_thread.shutdown_and_wait().await?;

    let resumed_request = mount_response_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, "inspect discarded child history"),
        sse_response(sse(vec![
            ev_response_created("resp-child-resumed"),
            ev_assistant_message("msg-child-resumed", "resume inspected"),
            ev_completed("resp-child-resumed"),
        ]))
        .set_delay(Duration::from_secs(1)),
    )
    .await;
    let resumed = test
        .thread_manager
        .resume_thread_from_rollout(
            test.config.clone(),
            child_rollout_path,
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?
        .thread;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    resumed
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "inspect discarded child history".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;
    let resumed_requests = wait_for_requests(&resumed_request).await?;
    assert!(
        resumed_requests[0]
            .message_input_texts("assistant")
            .iter()
            .all(|text| text != "child done"),
        "discarded child output was restored as assistant history: {:#?}",
        resumed_requests[0].body_json()
    );
    assert!(
        !resumed_requests[0].body_contains_text("msg-child-1"),
        "discarded child response id was restored into resumed model input: {:#?}",
        resumed_requests[0].body_json()
    );
    resumed.shutdown_and_wait().await?;

    assert_resumed_root_rejects_failed_terminal_delivery(
        &test,
        &server,
        parent_rollout_path,
        &assignment_id,
        "verify failed child delivery remains rejected after restart",
        "terminal result was rejected by parent",
    )
    .await?;

    // A successful receipt cannot upgrade a later runtime failure. The child remains failed,
    // persists a failed terminal-delivery state, and a resumed root cannot certify that receipt.
    let terminal_error_server = start_mock_server().await;
    let (terminal_error_test, terminal_error_child, terminal_error_assignment_id) =
        setup_typed_v2_child_with_delayed_completion(
            &terminal_error_server,
            Duration::from_secs(1),
            TypedChildDelayedCompletion::TerminalError,
        )
        .await?;
    let terminal_error_events = collect_events_through_turn_complete(&terminal_error_child).await?;
    assert!(
        !terminal_error_events.iter().any(|event| {
            matches!(event, EventMsg::AgentMessage(message) if message.message == "child done")
        }),
        "post-receipt runtime failure published child success: {terminal_error_events:#?}"
    );
    let terminal_error_completion = terminal_error_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .expect("post-receipt failure TurnComplete");
    assert!(
        terminal_error_completion.last_agent_message.is_none(),
        "post-receipt runtime failure surfaced drafted output: {terminal_error_completion:#?}"
    );
    assert!(
        terminal_error_completion.surfaced_result.is_none(),
        "post-receipt runtime failure surfaced a result: {terminal_error_completion:#?}"
    );
    assert!(
        terminal_error_completion.error.is_some(),
        "post-receipt runtime failure was upgraded to success: {terminal_error_completion:#?}"
    );
    assert!(matches!(
        terminal_error_child.agent_status().await,
        AgentStatus::Errored(_)
    ));
    terminal_error_child.flush_rollout().await?;
    let terminal_error_child_rollout = terminal_error_child
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected post-receipt failure child rollout"))?;
    let terminal_error_child_history =
        tokio::fs::read_to_string(&terminal_error_child_rollout).await?;
    assert!(
        !terminal_error_child_history.contains("msg-child-failed-draft")
            && !terminal_error_child_history.contains("child drafted output before failure"),
        "post-receipt failed output leaked into child rollout: {terminal_error_child_history}"
    );
    terminal_error_child.shutdown_and_wait().await?;
    terminal_error_test.codex.flush_rollout().await?;
    let terminal_error_parent_rollout = terminal_error_test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected post-receipt failure parent rollout"))?;
    terminal_error_test.codex.shutdown_and_wait().await?;
    assert_resumed_root_rejects_failed_terminal_delivery(
        &terminal_error_test,
        &terminal_error_server,
        terminal_error_parent_rollout,
        &terminal_error_assignment_id,
        "verify post-receipt runtime failure remains rejected after restart",
        "terminal result failed before successful completion",
    )
    .await?;

    // A correction may rebind the same agent path while the original turn is still awaiting a
    // model response. The old turn must retain its original attempt authority: it cannot submit a
    // receipt for the successor, auto-seal the successor, or publish output using successor state.
    let stale_server = start_mock_server().await;
    let (stale_test, stale_child, assignment_id) = setup_typed_v2_child_with_delayed_completion(
        &stale_server,
        Duration::from_secs(5),
        TypedChildDelayedCompletion::StaleReceipt(STALE_RECEIPT_CALL_ID),
    )
    .await?;
    let set_gate_args = serde_json::to_string(&json!({
        "assignment_id": assignment_id,
        "gate": "review",
        "status": "changes_requested",
        "reason": "exercise stale turn authority"
    }))?;
    mount_sse_once_match(
        &stale_server,
        |req: &wiremock::Request| body_contains(req, "amend while old child turn is delayed"),
        sse(vec![
            ev_response_created("resp-parent-set-gate"),
            ev_function_call_with_namespace(
                SET_REVIEW_GATE_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "set_agent_gate",
                &set_gate_args,
            ),
            ev_completed("resp-parent-set-gate"),
        ]),
    )
    .await;
    let amend_args = serde_json::to_string(&json!({
        "assignment_id": assignment_id,
        "reason": "create successor while original turn is still active"
    }))?;
    mount_sse_once_match(
        &stale_server,
        |req: &wiremock::Request| request_has_call_output(req, SET_REVIEW_GATE_CALL_ID),
        sse(vec![
            ev_response_created("resp-parent-amend"),
            ev_function_call_with_namespace(
                AMEND_TASK_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "amend_agent_task",
                &amend_args,
            ),
            ev_completed("resp-parent-amend"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &stale_server,
        |req: &wiremock::Request| request_has_call_output(req, AMEND_TASK_CALL_ID),
        sse(vec![
            ev_response_created("resp-parent-amended"),
            ev_assistant_message("msg-parent-amended", "successor attempt created"),
            ev_completed("resp-parent-amended"),
        ]),
    )
    .await;
    stale_test
        .submit_turn("amend while old child turn is delayed")
        .await?;

    let stale_events = collect_events_through_turn_complete(&stale_child).await?;
    assert!(
        !stale_events.iter().any(|event| {
            matches!(event, EventMsg::AgentMessage(message) if message.message == "stale child done")
        }),
        "stale child output must remain private: {stale_events:#?}"
    );
    let stale_completion = stale_events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnComplete(completion) => Some(completion),
            _ => None,
        })
        .expect("stale child TurnComplete");
    assert!(
        stale_completion.last_agent_message.is_none(),
        "{stale_completion:#?}"
    );
    assert!(
        stale_completion.surfaced_result.is_none(),
        "{stale_completion:#?}"
    );
    assert!(stale_completion.error.is_some(), "{stale_completion:#?}");
    assert!(matches!(
        stale_child.agent_status().await,
        AgentStatus::Errored(_)
    ));
    let stale_receipt_output = raw_tool_output(&stale_events, STALE_RECEIPT_CALL_ID)
        .ok_or_else(|| anyhow::anyhow!("missing stale receipt tool output"))?;
    assert!(
        stale_receipt_output.contains("no longer active"),
        "old turn unexpectedly submitted a successor receipt: {stale_receipt_output}"
    );
    for call_id in [
        STALE_GET_TASK_CALL_ID,
        STALE_SET_GATE_CALL_ID,
        STALE_SPAWN_CALL_ID,
    ] {
        let output = raw_tool_output(&stale_events, call_id)
            .ok_or_else(|| anyhow::anyhow!("missing stale tool output for {call_id}"))?;
        assert!(
            output.contains("no longer active"),
            "stale typed tool unexpectedly used successor authority: {output}"
        );
    }
    let stale_patch_output = raw_tool_output(&stale_events, STALE_PATCH_CALL_ID)
        .ok_or_else(|| anyhow::anyhow!("missing stale apply_patch output"))?;
    assert!(
        stale_patch_output.contains("no longer active"),
        "stale apply_patch unexpectedly used successor authority: {stale_patch_output}"
    );
    assert!(
        !stale_test
            .cwd_path()
            .join("stale-child-mutation.txt")
            .exists(),
        "stale typed turn mutated the successor workspace"
    );

    stale_child.flush_rollout().await?;
    let stale_rollout_path = stale_child
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected stale child rollout path"))?;
    let stale_rollout = tokio::fs::read_to_string(&stale_rollout_path).await?;
    assert!(
        !stale_rollout.contains("msg-child-stale") && !stale_rollout.contains("stale child done"),
        "stale output leaked into persisted rollout: {stale_rollout}"
    );
    stale_child.shutdown_and_wait().await?;
    let stale_resume_request = mount_sse_once_match(
        &stale_server,
        |req: &wiremock::Request| body_contains(req, "inspect stale child history"),
        sse(vec![
            ev_response_created("resp-stale-resumed"),
            ev_assistant_message("msg-stale-resumed", "stale resume inspected"),
            ev_completed("resp-stale-resumed"),
        ]),
    )
    .await;
    let stale_resumed = stale_test
        .thread_manager
        .resume_thread_from_rollout(
            stale_test.config.clone(),
            stale_rollout_path,
            stale_test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?
        .thread;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, stale_test.cwd_path());
    stale_resumed
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "inspect stale child history".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(stale_test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            },
        })
        .await?;
    let stale_resume_requests = wait_for_requests(&stale_resume_request).await?;
    assert!(
        stale_resume_requests[0]
            .message_input_texts("assistant")
            .iter()
            .all(|text| text != "stale child done"),
        "stale output was restored as assistant history: {:#?}",
        stale_resume_requests[0].body_json()
    );
    assert!(
        !stale_resume_requests[0].body_contains_text("msg-child-stale"),
        "stale response id was restored into resumed input: {:#?}",
        stale_resume_requests[0].body_json()
    );
    stale_resumed.shutdown_and_wait().await?;
    stale_test.codex.shutdown_and_wait().await?;

    Ok(())
}

#[derive(Clone, Copy)]
enum CompletionScenario {
    Completed,
    TerminalError,
}

#[test_case(CompletionScenario::Completed ; "completed")]
#[test_case(CompletionScenario::TerminalError ; "terminal_error")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_multi_agent_v2_completion_without_receipt_sends_error_message(
    scenario: CompletionScenario,
) -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let child_events = match scenario {
        CompletionScenario::Completed => vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ],
        CompletionScenario::TerminalError => vec![ev_response_created("resp-child-1")],
    };
    let child_request = mount_response_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TASK_CAPSULE_OPEN_TAG) && body_contains(req, CHILD_PROMPT)
        },
        sse_response(sse(child_events)).set_delay(Duration::from_secs(1)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID) && !body_contains(req, "Message Type: FINAL_ANSWER")
        },
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "parent done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;
    let error = "Error while reading the server response: stream closed before response.completed";
    let (status, expected_text) = match scenario {
        CompletionScenario::Completed => {
            ("Completed(Some(\"child done\"))".to_string(), "child done")
        }
        CompletionScenario::TerminalError => (format!("Errored(\"{error}\")"), error),
    };
    let payload = format!(
        "Agent errored: durable typed receipt status: needs_main: typed agent /root/worker finished with status {status} without submitting a receipt"
    );
    let notification = format!(
        concat!(
            "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\n{payload}\n\n",
            "This agent's turn failed. The full sealed error remains available through get_agent_task; ",
            "retrieve it with the assignment id returned by spawn_agent before deciding whether to retry. ",
            "If you still need this agent, use the available collaboration tools to give it another task."
        ),
        payload = payload,
    );
    // If the child is still running when the parent turn starts, wait_agent blocks
    // until mailbox delivery. The follow-up request must then contain that delivery.
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_2_NO_WAIT_PROMPT)
                && !body_contains(req, "Message Type: FINAL_ANSWER")
        },
        sse(vec![
            ev_response_created("resp-parent-3"),
            ev_function_call_with_namespace(
                "wait-agent-call",
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("resp-parent-3"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_2_NO_WAIT_PROMPT)
                && request_has_call_output(req, "wait-agent-call")
                && !request_has_input_type(req, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-parent-4"),
            ev_function_call_with_namespace(
                "wait-agent-call-2",
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("resp-parent-4"),
        ]),
    )
    .await;
    let agent_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_2_NO_WAIT_PROMPT)
                && request_has_input_type(req, "agent_message")
                && body_contains(req, expected_text)
        },
        sse(vec![
            ev_response_created("resp-parent-5"),
            ev_assistant_message("msg-parent-5", "done"),
            ev_completed("resp-parent-5"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_model("koffing")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build(&server)
        .await?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = wait_for_requests(&child_request).await?;
    test.submit_turn(TURN_2_NO_WAIT_PROMPT).await?;

    let request = wait_for_requests(&agent_request)
        .await?
        .pop()
        .expect("agent message request");
    let mut agent_messages = request.inputs_of_type("agent_message");
    assert_eq!(
        agent_messages.len(),
        1,
        "completion request inputs: {:#}",
        Value::Array(request.input())
    );

    let agent_message = &mut agent_messages[0];
    let (id, metadata) = {
        let object = agent_message
            .as_object_mut()
            .expect("agent message should be an object");
        (
            object.remove("id"),
            object
                .remove("internal_chat_message_metadata_passthrough")
                .expect("turn metadata"),
        )
    };

    if let Some(id) = id {
        let id = id.as_str().expect("terminal notification ID string");
        assert!(id.starts_with("amsg_"));
    }

    let metadata = metadata.as_object().expect("turn metadata object");
    assert_eq!(metadata.len(), 1);
    assert!(
        metadata
            .get("turn_id")
            .and_then(Value::as_str)
            .is_some_and(|turn_id| !turn_id.is_empty())
    );

    assert_eq!(
        agent_message,
        &json!({
            "type": "agent_message",
            "author": "/root/worker",
            "recipient": "/root",
            "content": [{
                "type": "input_text",
                "text": notification,
            }],
        })
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skills_toggle_skips_instructions_for_parent_and_spawned_child() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "worker",
    }))?;
    let spawn_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            write_home_skill(home, "demo", "demo-skill", "demo skill").expect("write home skill");
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.include_skill_instructions = false;
        });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let parent_request = spawn_turn.single_request();
    assert!(!parent_request.body_contains_text("<skills_instructions>"));
    assert!(!parent_request.body_contains_text("demo-skill"));

    let child_requests = wait_for_requests(&child_request_log).await?;
    let child_request = child_requests
        .last()
        .expect("child request log should capture at least one request");
    assert!(!child_request.body_contains_text("<skills_instructions>"));
    assert!(!child_request.body_contains_text("demo-skill"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_role_overrides_requested_model_and_reasoning_settings() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "agent_type": "custom",
            "model": REQUESTED_MODEL,
            "reasoning_effort": REQUESTED_REASONING_EFFORT,
        }),
        |builder| {
            builder.with_config(|config| {
                let role_path = config.codex_home.join("custom-role.toml");
                std::fs::write(
                    &role_path,
                    format!(
                        "model = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"{ROLE_REASONING_EFFORT}\"\n",
                    ),
                )
                .expect("write role config");
                config.agent_roles.insert(
                    "custom".to_string(),
                    AgentRoleConfig {
                        description: Some("Custom role".to_string()),
                        config_file: Some(role_path.to_path_buf()),
                        nickname_candidates: None,
                    },
                );
            })
        },
    )
    .await?;

    assert_eq!(child_snapshot.model, ROLE_MODEL);
    assert_eq!(child_snapshot.reasoning_effort, Some(ROLE_REASONING_EFFORT));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_tool_description_mentions_role_locked_settings() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let resp_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1"),
            ev_assistant_message("msg-turn1", "done"),
            ev_completed("resp-turn1"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model("gpt-5.4")
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.supports_search_tool = false;
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("test config should select the V1 tool contract");
            config.multi_agent_v2.hide_spawn_agent_metadata = false;
            let role_path = config.codex_home.join("custom-role.toml");
            std::fs::write(
                &role_path,
                format!(
                    "developer_instructions = \"Stay focused\"\nmodel = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"{ROLE_REASONING_EFFORT}\"\n",
                ),
            )
            .expect("write role config");
            config.agent_roles.insert(
                "custom".to_string(),
                AgentRoleConfig {
                    description: Some("Custom role".to_string()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_1_PROMPT).await?;

    let request = resp_mock.single_request();
    let spawn_agent = request
        .tool_by_name("multi_agent_v1", "spawn_agent")
        .expect("request should expose multi_agent_v1.spawn_agent");
    let agent_type_description = tool_parameter_description(&spawn_agent, "agent_type")
        .unwrap_or_else(|| panic!("spawn_agent agent_type description: {spawn_agent:#}"));
    let custom_role_description =
        role_block(&agent_type_description, "custom").expect("custom role description");
    assert_eq!(
        custom_role_description,
        "custom: {\nCustom role\n- This role's model is set to `gpt-5.4` and its reasoning effort is set to `high`. These settings cannot be changed.\n}"
    );

    Ok(())
}
