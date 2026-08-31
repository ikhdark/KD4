use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::assert_regex_match;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use serde_json::json;
use test_case::test_case;

const DEFAULT_SHELL_TIMEOUT_MS: i64 = 7_000;

const MEDIUM_TIMEOUT: Duration = Duration::from_secs(10);

fn shell_responses_with_timeout(
    call_id: &str,
    command: &str,
    login: Option<bool>,
    timeout_ms: i64,
) -> Vec<String> {
    shell_responses_with_deadlines(call_id, command, login, timeout_ms, None)
}

fn shell_responses_with_deadlines(
    call_id: &str,
    command: &str,
    login: Option<bool>,
    timeout_ms: i64,
    stall_timeout_ms: Option<i64>,
) -> Vec<String> {
    let args = json!({
        "kind": "script",
        "command": command,
        "timeout_ms": timeout_ms,
        "stall_timeout_ms": stall_timeout_ms,
        "login": login,
    });

    let arguments = serde_json::to_string(&args).expect("serialize shell command arguments");

    vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &arguments),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    ]
}

fn shell_responses(call_id: &str, command: &str, login: Option<bool>) -> Vec<String> {
    shell_responses_with_timeout(call_id, command, login, DEFAULT_SHELL_TIMEOUT_MS)
}

async fn shell_command_harness_with(
    configure: impl FnOnce(TestCodexBuilder) -> TestCodexBuilder,
) -> Result<TestCodexHarness> {
    let builder = configure(test_codex().with_raw_response_items());
    TestCodexHarness::with_builder(builder).await
}

async fn mount_shell_responses(
    harness: &TestCodexHarness,
    call_id: &str,
    command: &str,
    login: Option<bool>,
) {
    mount_sse_sequence(harness.server(), shell_responses(call_id, command, login)).await;
}

async fn mount_terminal_shell_response(harness: &TestCodexHarness, responses: Vec<String>) {
    let response = responses
        .into_iter()
        .next()
        .expect("terminal shell fixture should include a tool-call response");
    mount_sse_sequence(harness.server(), vec![response]).await;
}

async fn mount_shell_responses_with_timeout(
    harness: &TestCodexHarness,
    call_id: &str,
    command: &str,
    login: Option<bool>,
    timeout: Duration,
) {
    mount_sse_sequence(
        harness.server(),
        shell_responses_with_timeout(call_id, command, login, timeout.as_millis() as i64),
    )
    .await;
}

async fn submit_shell_and_capture_output(
    harness: &TestCodexHarness,
    prompt: &str,
    call_id: &str,
) -> Result<String> {
    submit_shell_and_capture_output_and_begin(harness, prompt, call_id)
        .await
        .map(|(output, _)| output)
}

async fn submit_shell_and_capture_output_and_begin(
    harness: &TestCodexHarness,
    prompt: &str,
    call_id: &str,
) -> Result<(String, bool)> {
    let test = harness.test();
    let codex = test.codex.clone();
    let session_model = test.session_configured.model.clone();
    let cwd = test.config.cwd.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.as_path());
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd)),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    let deadline = tokio::time::Instant::now() + MEDIUM_TIMEOUT;
    let mut output = None;
    let mut saw_exec_command_begin = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "timed out waiting for raw shell output for {call_id}"
        );
        let event = tokio::time::timeout(remaining, codex.next_event())
            .await
            .context("timed out waiting for shell command completion")??;
        match event.msg {
            EventMsg::ExecCommandBegin(begin) if begin.call_id == call_id => {
                saw_exec_command_begin = true;
            }
            EventMsg::RawResponseItem(raw) => {
                if let ResponseItem::FunctionCallOutput {
                    call_id: output_call_id,
                    output: payload,
                    ..
                } = raw.item
                    && output_call_id == call_id
                    && let Some(text) = payload.text_content()
                {
                    output = Some(text.to_string());
                }
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    output
        .with_context(|| format!("raw function_call_output {call_id} not found"))
        .map(|output| (output, saw_exec_command_begin))
}

fn assert_shell_command_output(output: &str, expected: &str) -> Result<()> {
    let normalized_output = output
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_string();

    let expected_pattern = format!(
        r"(?s)^Exit code: 0\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nRaw output artifact: .+? \([0-9]+ bytes retained\)\nOutput:\n{expected}\n?$"
    );

    assert_regex_match(&expected_pattern, &normalized_output);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_works() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.4")).await?;

    let call_id = "shell-command-call";
    mount_shell_responses(
        &harness,
        call_id,
        "echo 'hello, world'",
        /*login*/ None,
    )
    .await;
    harness.submit("run the echo command").await?;

    let output = harness.function_call_stdout(call_id).await;
    assert_shell_command_output(&output, "hello, world")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_with_login() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.4")).await?;

    let call_id = "shell-command-call-login-true";
    mount_shell_responses(&harness, call_id, "echo 'hello, world'", Some(true)).await;
    harness.submit("run the echo command with login").await?;

    let output = harness.function_call_stdout(call_id).await;
    assert_shell_command_output(&output, "hello, world")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_without_login() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.4")).await?;

    let call_id = "shell-command-call-login-false";
    mount_shell_responses(&harness, call_id, "echo 'hello, world'", Some(false)).await;
    harness.submit("run the echo command without login").await?;

    let output = harness.function_call_stdout(call_id).await;
    assert_shell_command_output(&output, "hello, world")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_line_output_with_login() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.4")).await?;

    let call_id = "shell-command-call-first-extra-login";
    mount_shell_responses(
        &harness,
        call_id,
        "echo 'first line\nsecond line'",
        Some(true),
    )
    .await;
    harness.submit("run the command with login").await?;

    let output = harness.function_call_stdout(call_id).await;
    assert_shell_command_output(&output, "first line\nsecond line")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_times_out_with_timeout_ms() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.4")).await?;
    let call_id = "shell-command-timeout";
    let command = "powershell.exe -NoProfile -Command \"Start-Sleep -Seconds 5\"";
    mount_terminal_shell_response(
        &harness,
        shell_responses_with_timeout(call_id, command, /*login*/ None, 200),
    )
    .await;
    let output = submit_shell_and_capture_output(
        &harness,
        "run a long command with a short timeout",
        call_id,
    )
    .await?;
    let normalized_output = output
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_string();
    let expected_pattern = r"(?s)^Exit code: 124\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nRaw output artifact: .+? \([0-9]+ bytes retained\)\nOutput:\ncommand timed out after [0-9]+ milliseconds\n?$";
    assert_regex_match(expected_pattern, &normalized_output);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_cancels_after_output_stalls() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.4")).await?;
    let call_id = "shell-command-stall-timeout";
    let command = "powershell.exe -NoProfile -Command \"Start-Sleep -Seconds 5\"";
    mount_terminal_shell_response(
        &harness,
        shell_responses_with_deadlines(call_id, command, /*login*/ None, 5_000, Some(200)),
    )
    .await;
    let output = submit_shell_and_capture_output(
        &harness,
        "run a command with a short stall deadline",
        call_id,
    )
    .await?;
    let normalized_output = output.replace("\r\n", "\n").replace('\r', "\n");
    let expected_pattern = r"(?s)^Exit code: 124\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nRaw output artifact: .+? \([0-9]+ bytes retained\)\nOutput:\ncommand timed out after [0-9]+ milliseconds\n.*command stalled after 200 milliseconds without stdout or stderr\n?$";
    assert!(
        regex_lite::Regex::new(expected_pattern)
            .expect("stall output regex is valid")
            .is_match(normalized_output.trim_end_matches('\n')),
        "stall output did not match the expected timeout contract: {normalized_output:?}",
    );

    Ok(())
}

/// This test verifies that a shell, particularly PowerShell, can correctly
/// handle unicode output when the UTF-8 BOM is used. See
/// https://github.com/openai/codex/pull/7902 for more context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case(true ; "with_login")]
#[test_case(false ; "without_login")]
async fn unicode_output(login: bool) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.2")).await?;

    let call_id = "unicode_output";
    let command = // We use a child process on Windows instead of a PowerShell command
        // like `Write-Output` to ensure that the Powershell config is set
        // correctly.
        "cmd.exe /c echo naïve_café";
    mount_shell_responses_with_timeout(&harness, call_id, command, Some(login), MEDIUM_TIMEOUT)
        .await;
    harness.submit("run the command without login").await?;

    let output = harness.function_call_stdout(call_id).await;
    assert_shell_command_output(&output, "naïve_café")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case(true ; "with_login")]
#[test_case(false ; "without_login")]
async fn unicode_output_with_newlines(login: bool) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = shell_command_harness_with(|builder| builder.with_model("gpt-5.2")).await?;

    let call_id = "unicode_output";
    mount_shell_responses_with_timeout(
        &harness,
        call_id,
        "echo 'line1\nnaïve café\nline3'",
        Some(login),
        MEDIUM_TIMEOUT,
    )
    .await;
    harness.submit("run the command without login").await?;

    let output = harness.function_call_stdout(call_id).await;
    assert_shell_command_output(&output, "line1\\nnaïve café\\nline3")?;

    Ok(())
}
