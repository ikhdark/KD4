use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use tempfile::TempDir;

fn code_mode_custom_tool_output_text(output_item: &Value) -> String {
    match output_item.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(output)) => output
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        output => panic!("unexpected code mode custom tool output: {output:?}"),
    }
}

fn powershell_single_quoted(value: &str) -> String {
    value.replace("'", "''")
}

fn marker_command(marker: &Path, marker_value: &str, output: &str) -> String {
    let marker = powershell_single_quoted(&marker.display().to_string());
    format!(
        concat!(
            "powershell.exe -NoProfile -NonInteractive -Command ",
            "\"Set-Content -NoNewline -LiteralPath '{marker}' -Value '{marker_value}'; ",
            "[Console]::Write('{output}')\""
        ),
        marker = marker,
        marker_value = marker_value,
        output = output
    )
}

fn write_updating_pre_tool_use_hook(
    home: &Path,
    matcher: &str,
    updated_input: &Value,
) -> Result<()> {
    let script_path = home.join("pre_tool_use_hook.ps1");
    let log_path = home.join("pre_tool_use_hook_log.jsonl");
    let hook_output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated_input,
        }
    })
    .to_string();
    let script = format!(
        concat!(
            "$payload = [Console]::In.ReadToEnd()\n",
            "Add-Content -LiteralPath '{log_path}' -Value $payload\n",
            "[Console]::Out.WriteLine('{hook_output}')\n"
        ),
        log_path = powershell_single_quoted(&log_path.display().to_string()),
        hook_output = powershell_single_quoted(&hook_output),
    );
    let hook_command = format!(
        "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        script_path.display()
    );
    let hooks = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": matcher,
                "hooks": [{
                    "type": "command",
                    "command": hook_command,
                    "commandWindows": hook_command,
                    "statusMessage": "rewriting pre tool input",
                }]
            }]
        }
    });

    fs::write(&script_path, script).context("write Windows pre tool use hook script")?;
    fs::write(home.join("hooks.json"), hooks.to_string()).context("write hooks.json")?;
    Ok(())
}

fn read_pre_tool_use_hook_inputs(home: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(home.join("pre_tool_use_hook_log.jsonl"))?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("parse pre tool use hook input"))
        .collect()
}

#[test]
fn pre_tool_use_rewrites_code_mode_nested_exec_command_before_execution() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .name("pre_tool_use_rewrites_code_mode_nested_exec_command_before_execution".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_stack_size(TEST_STACK_SIZE_BYTES)
                .enable_all()
                .build()?;
            runtime.block_on(
                pre_tool_use_rewrites_code_mode_nested_exec_command_before_execution_impl(),
            )
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "pre_tool_use_rewrites_code_mode_nested_exec_command_before_execution thread panicked"
        )),
    }
}

async fn pre_tool_use_rewrites_code_mode_nested_exec_command_before_execution_impl() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "pretooluse-code-mode-rewrite-windows";
    let marker_dir = TempDir::new().context("create pre tool rewrite marker directory")?;
    let original_marker = marker_dir.path().join("original");
    let rewritten_marker = marker_dir.path().join("rewritten");
    let original_command = marker_command(&original_marker, "original", "original-result");
    let rewritten_command = marker_command(&rewritten_marker, "rewritten", "rewritten-result");
    let original_command_json =
        serde_json::to_string(&original_command).context("serialize original command")?;
    let code = format!(
        r#"
let output = await tools.exec_command({{ kind: "script", cmd: {original_command_json} }});
while (output.session_id) {{
  output = await tools.write_stdin({{
    session_id: output.session_id,
    chars: "",
    yield_time_ms: 30_000,
  }});
}}
if (output.raw_output_artifact_id) {{
  const recovered = await tools.read_tool_output({{
    artifact_id: output.raw_output_artifact_id,
    start_line: 1,
    end_line: 1,
  }});
  output.output = recovered.results.map(part => part.text ?? "").join("");
}}
text(output.output);
"#
    );
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_custom_tool_call(call_id, "exec", &code),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "hook rewrote the nested command"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let updated_input = serde_json::json!({ "command": rewritten_command });
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_pre_build_hook(move |home| {
            write_updating_pre_tool_use_hook(home, "^Bash$", &updated_input)
                .expect("failed to write Windows updating pre tool use hook fixture");
        })
        .with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::UnifiedExec);
            trust_discovered_hooks(config);
        });
    let test = builder.build(&server).await?;

    test.submit_turn_with_permission_profile(
        "run the rewritten shell command from code mode",
        PermissionProfile::Disabled,
    )
    .await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    let output_item = requests[1].custom_tool_call_output(call_id);
    let output = code_mode_custom_tool_output_text(&output_item);
    let hook_log = fs::read_to_string(test.codex_home_path().join("pre_tool_use_hook_log.jsonl"))
        .unwrap_or_else(|error| format!("<hook log unavailable: {error}>"));
    assert!(
        output.contains("rewritten-result"),
        concat!(
            "code mode should receive the rewritten command result; ",
            "output={:?}; original_marker_exists={}; ",
            "rewritten_marker_exists={}; hook_log={}"
        ),
        output,
        original_marker.exists(),
        rewritten_marker.exists(),
        hook_log,
    );
    assert!(
        !output.contains("original-result"),
        "code mode should not receive the original command result"
    );
    assert!(
        !original_marker.exists(),
        "original nested shell command should not execute after rewrite"
    );
    assert_eq!(
        fs::read_to_string(&rewritten_marker)
            .context("read rewritten code mode pre tool marker")?,
        "rewritten"
    );

    let hook_inputs = read_pre_tool_use_hook_inputs(test.codex_home_path())?;
    assert_eq!(hook_inputs.len(), 1);
    assert_eq!(hook_inputs[0]["tool_input"]["command"], original_command);

    Ok(())
}
