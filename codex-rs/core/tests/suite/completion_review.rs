use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use serde_json::Value;
use serde_json::json;
use tokio::time::sleep;
use tokio::time::timeout;

const REVIEW_REQUEST_MARKER: &str = "KD4_COMPLETION_REVIEW_REQUEST_V1";
const REPAIR_MARKER: &str = "<kd4_completion_repair>";

fn completion_review_builder() -> TestCodexBuilder {
    completion_review_builder_with_role(true)
}

fn completion_review_builder_with_role(register_reviewer_role: bool) -> TestCodexBuilder {
    test_codex().with_config(move |config| {
        fs::create_dir_all(config.cwd.join(".git")).expect("create git marker");
        fs::write(
            config.cwd.join("kd4_features.toml"),
            "schema_version = 1\nfork = \"KD4\"\n",
        )
        .expect("write KD4 marker");
        if register_reviewer_role {
            let reviewer_role = config.codex_home.join("kd4-reviewer-test.toml");
            fs::write(
                &reviewer_role,
                "model_reasoning_effort = \"high\"\nsandbox_mode = \"read-only\"\n",
            )
            .expect("write reviewer role");
            config.agent_roles.insert(
                "kd4_reviewer".to_string(),
                AgentRoleConfig {
                    description: Some("KD4 completion reviewer".to_string()),
                    config_file: Some(reviewer_role.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        }
        config
            .features
            .enable(Feature::TaskCompletionReviewer)
            .expect("enable completion reviewer");
    })
}

fn write_explicit_stop_hook(home: &Path) {
    let (script_path, command) = if cfg!(windows) {
        let script_path = home.join("completion-review-stop.ps1");
        fs::write(
            &script_path,
            r#"[Console]::In.ReadToEnd() | Out-Null
Write-Output '{"continue":false,"stopReason":"explicit stop"}'
"#,
        )
        .expect("write explicit stop hook");
        (
            script_path.clone(),
            format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
                script_path.display()
            ),
        )
    } else {
        let script_path = home.join("completion-review-stop.sh");
        fs::write(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"continue\":false,\"stopReason\":\"explicit stop\"}'\n",
        )
        .expect("write explicit stop hook");
        (
            script_path.clone(),
            format!("sh \"{}\"", script_path.display()),
        )
    };
    assert!(script_path.is_file());
    fs::write(
        home.join("hooks.json"),
        json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": command
                    }]
                }]
            }
        })
        .to_string(),
    )
    .expect("write explicit stop hook config");
}

fn write_single_continuation_stop_hook(home: &Path) {
    let prompt = "complete the stop-hook-requested follow-up";
    let (script_path, command) = if cfg!(windows) {
        let script_path = home.join("completion-review-continuation.ps1");
        fs::write(
            &script_path,
            format!(
                r#"[Console]::In.ReadToEnd() | Out-Null
$state = Join-Path $PSScriptRoot 'completion-review-stop-seen'
if (Test-Path -LiteralPath $state) {{
    Write-Output '{{}}'
}} else {{
    New-Item -ItemType File -Path $state | Out-Null
    Write-Output '{{"decision":"block","reason":"{prompt}"}}'
}}
"#
            ),
        )
        .expect("write continuation stop hook");
        (
            script_path.clone(),
            format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
                script_path.display()
            ),
        )
    } else {
        let script_path = home.join("completion-review-continuation.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nstate=\"$(dirname \"$0\")/completion-review-stop-seen\"\nif [ -f \"$state\" ]; then\n  printf '%s\\n' '{{}}'\nelse\n  : > \"$state\"\n  printf '%s\\n' '{{\"decision\":\"block\",\"reason\":\"{prompt}\"}}'\nfi\n"
            ),
        )
        .expect("write continuation stop hook");
        (
            script_path.clone(),
            format!("sh \"{}\"", script_path.display()),
        )
    };
    assert!(script_path.is_file());
    fs::write(
        home.join("hooks.json"),
        json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": command
                    }]
                }]
            }
        })
        .to_string(),
    )
    .expect("write continuation stop hook config");
}

fn completion_review_builder_with_after_agent_probe() -> TestCodexBuilder {
    completion_review_builder().with_config(|config| {
        let marker = config.codex_home.join("after-agent-order.txt");
        if cfg!(windows) {
            let script = config.codex_home.join("after-agent-order.ps1");
            fs::write(
                &script,
                r#"param([string]$Payload)
$receipt = Get-ChildItem -Path (Join-Path $PSScriptRoot 'task-evidence\*.json') -ErrorAction SilentlyContinue | Select-Object -First 1
$result = 'missing-review'
if ($null -ne $receipt) {
    $content = Get-Content -LiteralPath $receipt.FullName -Raw
    if ($content -match '"completion_review_receipts"\s*:\s*\[\s*\{' -and $content -match '"outcome"\s*:\s*"clean"') {
        $result = 'reviewed'
    }
}
Set-Content -LiteralPath (Join-Path $PSScriptRoot 'after-agent-order.txt') -Value $result
"#,
            )
            .expect("write AfterAgent probe");
            config.notify = Some(vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                script.display().to_string(),
            ]);
        } else {
            let script = config.codex_home.join("after-agent-order.sh");
            fs::write(
                &script,
                "#!/bin/sh\nif grep -Eq '\"completion_review_receipts\"[[:space:]]*:[[:space:]]*\\[[[:space:]]*\\{' \"$(dirname \"$0\")\"/task-evidence/*.json && grep -Eq '\"outcome\"[[:space:]]*:[[:space:]]*\"clean\"' \"$(dirname \"$0\")\"/task-evidence/*.json; then\n  printf reviewed > \"$(dirname \"$0\")/after-agent-order.txt\"\nelse\n  printf missing-review > \"$(dirname \"$0\")/after-agent-order.txt\"\nfi\n",
            )
            .expect("write AfterAgent probe");
            config.notify = Some(vec!["sh".to_string(), script.display().to_string()]);
        }
        assert_eq!(marker.file_name().and_then(|name| name.to_str()), Some("after-agent-order.txt"));
    })
}

fn plan_response(response_id: &str, call_id: &str, status: &str) -> String {
    let args = json!({
        "explanation": "completion review test",
        "plan": [{
            "id": "completion-step",
            "step": "Implement the requested completion behavior",
            "status": status,
            "depends_on": [],
            "acceptance_criteria": ["The requested file behavior is present"],
            "runtime_paths": [],
            "generated_artifacts": [],
            "risks": [],
            "requires_desktop_activation": false
        }]
    })
    .to_string();
    sse(vec![
        ev_response_created(response_id),
        ev_function_call(call_id, "update_plan", &args),
        ev_completed(response_id),
    ])
}

fn patch_response(response_id: &str, call_id: &str, patch: &str) -> String {
    sse(vec![
        ev_response_created(response_id),
        ev_apply_patch_custom_tool_call(call_id, patch),
        ev_completed(response_id),
    ])
}

fn message_response(response_id: &str, message_id: &str, text: &str) -> String {
    sse(vec![
        ev_response_created(response_id),
        ev_assistant_message(message_id, text),
        ev_completed(response_id),
    ])
}

fn clean_review_response(response_id: &str) -> String {
    message_response(
        response_id,
        "review-clean-message",
        &json!({"verdict": "clean", "findings": []}).to_string(),
    )
}

fn repair_review_response(response_id: &str, summary: &str) -> String {
    message_response(
        response_id,
        "review-repair-message",
        &json!({
            "verdict": "repair_needed",
            "findings": [{
                "severity": "high",
                "summary": summary,
                "evidence": "the candidate omitted the stated requirement",
                "smallest_correction": "add the omitted requirement to completed.txt",
                "proof_command": "cargo test -p codex-core completion_review"
            }]
        })
        .to_string(),
    )
}

fn reviewer_request_count(requests: &[core_test_support::responses::ResponsesRequest]) -> usize {
    requests
        .iter()
        .filter(|request| request.body_contains_text(REVIEW_REQUEST_MARKER))
        .count()
}

fn text_occurrences(value: &Value, needle: &str) -> usize {
    match value {
        Value::String(text) => text.matches(needle).count(),
        Value::Array(values) => values
            .iter()
            .map(|value| text_occurrences(value, needle))
            .sum(),
        Value::Object(values) => values
            .values()
            .map(|value| text_occurrences(value, needle))
            .sum(),
        _ => 0,
    }
}

async fn submit_turn_and_capture_completion(
    test: &TestCodex,
    prompt: &str,
) -> Result<TurnCompleteEvent> {
    submit_turn_and_capture_completion_with(test, prompt, ModeKind::Default, None).await
}

async fn submit_turn_and_capture_completion_with(
    test: &TestCodex,
    prompt: &str,
    mode: ModeKind,
    final_output_json_schema: Option<Value>,
) -> Result<TurnCompleteEvent> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                environments: None,
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    loop {
        let event = timeout(Duration::from_secs(30), test.codex.next_event()).await??;
        if let EventMsg::TurnComplete(completed) = event.msg {
            return Ok(completed);
        }
    }
}

async fn assert_no_additional_turn_complete(test: &TestCodex) {
    let additional = timeout(Duration::from_millis(100), async {
        loop {
            let Ok(event) = test.codex.next_event().await else {
                return false;
            };
            if matches!(event.msg, EventMsg::TurnComplete(_)) {
                return true;
            }
        }
    })
    .await;
    assert!(!matches!(additional, Ok(true)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_review_finishes_without_a_repair_continuation() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            clean_review_response("review-clean"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested completion behavior")
            .await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    assert_no_additional_turn_complete(&test).await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(reviewer_request_count(&requests), 1);
    assert!(
        !requests
            .iter()
            .any(|request| request.body_contains_text(REPAIR_MARKER))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_finding_injects_one_repair_and_repair_mutation_cannot_rearm_review() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let finding = "the user requirement was omitted";
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            repair_review_response("review-repair", finding),
            patch_response(
                "repair-patch",
                "repair-patch-call",
                "*** Begin Patch\n*** Update File: completed.txt\n@@\n-done\n+done with omitted requirement\n*** End Patch",
            ),
            plan_response("repair-plan-pass", "repair-plan-pass-call", "passed"),
            message_response("repaired", "repaired-message", "repair complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement every stated requirement").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    assert_no_additional_turn_complete(&test).await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 8);
    assert_eq!(reviewer_request_count(&requests), 1);
    let repair_requests = requests
        .iter()
        .filter(|request| request.body_contains_text(REPAIR_MARKER))
        .collect::<Vec<_>>();
    assert_eq!(repair_requests.len(), 3);
    assert!(
        repair_requests
            .iter()
            .all(|request| text_occurrences(&request.body_json(), REPAIR_MARKER) == 1)
    );
    assert!(repair_requests[0].body_contains_text(finding));
    assert!(test.workspace_path("completed.txt").is_file());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonpassed_evidence_triggers_repair_even_when_review_is_clean() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            message_response("candidate", "candidate-message", "implementation complete"),
            clean_review_response("review-clean"),
            plan_response("repair-plan-pass", "repair-plan-pass-call", "passed"),
            message_response("repaired", "repaired-message", "evidence repaired"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement and prove the completion behavior")
            .await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(reviewer_request_count(&requests), 1);
    assert!(
        requests
            .iter()
            .any(|request| request.body_contains_text("Evidence gap:"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_and_evidence_findings_share_the_single_repair_fragment() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let finding = "reviewer-specific omitted requirement";
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            message_response("candidate", "candidate-message", "implementation complete"),
            repair_review_response("review-repair", finding),
            plan_response("repair-plan-pass", "repair-plan-pass-call", "passed"),
            message_response("repaired", "repaired-message", "combined repair complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion = submit_turn_and_capture_completion(
        &test,
        "Implement every requirement and prove completion",
    )
    .await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );

    let requests = response_mock.requests();
    assert_eq!(reviewer_request_count(&requests), 1);
    let repair_request = requests
        .iter()
        .find(|request| request.body_contains_text(REPAIR_MARKER))
        .expect("single repair request");
    assert!(repair_request.body_contains_text(finding));
    assert!(repair_request.body_contains_text("Evidence gap:"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_reviewer_output_is_partial_and_never_blocked() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            message_response("review-malformed", "review-message", "not-json"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let gate = completion.completion.expect("completion report");
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("malformed"))
    );
    assert_eq!(reviewer_request_count(&response_mock.requests()), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_reviewer_output_is_partial_and_never_blocked() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            message_response(
                "review-oversized",
                "review-message",
                &format!(
                    "{{\"verdict\":\"clean\",\"findings\":[],\"padding\":\"{}\"}}",
                    "word ".repeat(10_000)
                ),
            ),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let gate = completion.completion.expect("completion report");
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("2,000-token limit"))
    );
    assert_eq!(reviewer_request_count(&response_mock.requests()), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_spawn_failure_is_partial_and_never_blocked() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder_with_role(false);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let gate = completion.completion.expect("completion report");
    assert_eq!(gate.status, TaskCompletionStatus::Partial);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("could not start or complete"))
    );
    assert_eq!(reviewer_request_count(&response_mock.requests()), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_stop_exits_before_reviewer_invocation() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder()
        .with_pre_build_hook(write_explicit_stop_hook)
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_continuation_runs_before_the_single_reviewer() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let continuation = "complete the stop-hook-requested follow-up";
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            message_response(
                "stop-continuation",
                "stop-continuation-message",
                "follow-up complete",
            ),
            clean_review_response("review-clean"),
        ],
    )
    .await;
    let mut builder = completion_review_builder()
        .with_pre_build_hook(write_single_continuation_stop_hook)
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(reviewer_request_count(&requests), 1);
    assert!(
        requests
            .iter()
            .any(|request| request.body_contains_text(continuation))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_finishes_before_legacy_after_agent_hook() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
            clean_review_response("review-clean"),
        ],
    )
    .await;
    let mut builder = completion_review_builder_with_after_agent_probe();
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    assert_eq!(reviewer_request_count(&response_mock.requests()), 1);

    let marker = test.home.path().join("after-agent-order.txt");
    for _ in 0..100 {
        if marker.is_file() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fs::read_to_string(marker)?.trim(), "reviewed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_feature_skips_review_without_changing_the_evidence_gate() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder().with_config(|config| {
        config
            .features
            .disable(Feature::TaskCompletionReviewer)
            .expect("disable completion reviewer");
    });
    let test = builder.build(&server).await?;

    let completion =
        submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    assert_eq!(
        completion.completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_turn_skips_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion(&test, "Inspect the requested behavior").await?;
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_kd4_repository_skips_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder().with_config(|config| {
        fs::remove_file(config.cwd.join("kd4_features.toml")).expect("remove KD4 marker");
    });
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion(&test, "Implement the requested behavior").await?;
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_skips_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "implementation complete"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion_with(
        &test,
        "Plan the requested behavior",
        ModeKind::Plan,
        None,
    )
    .await?;
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_output_turn_skips_review() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            plan_response("plan-start", "plan-start-call", "in_progress"),
            patch_response(
                "initial-patch",
                "initial-patch-call",
                "*** Begin Patch\n*** Add File: completed.txt\n+done\n*** End Patch",
            ),
            plan_response("plan-pass", "plan-pass-call", "passed"),
            message_response("candidate", "candidate-message", "{}"),
        ],
    )
    .await;
    let mut builder = completion_review_builder();
    let test = builder.build(&server).await?;

    submit_turn_and_capture_completion_with(
        &test,
        "Implement the requested behavior",
        ModeKind::Default,
        Some(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })),
    )
    .await?;
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(reviewer_request_count(&requests), 0);
    Ok(())
}
