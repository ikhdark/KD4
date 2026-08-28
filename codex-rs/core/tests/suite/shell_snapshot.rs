use anyhow::Result;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;

async fn wait_for_snapshot(codex_home: &Path) -> Result<PathBuf> {
    let snapshot_dir = codex_home.join("shell_snapshots");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut entries) = fs::read_dir(&snapshot_dir).await {
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("ps1") {
                    return Ok(path);
                }
            }
        }

        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for PowerShell snapshot");
        }

        sleep(Duration::from_millis(25)).await;
    }
}

async fn run_tool_turn_on_harness(
    harness: &TestCodexHarness,
    prompt: &str,
    call_id: &str,
    args: serde_json::Value,
) -> Result<ExecCommandEndEvent> {
    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    ];
    mount_sse_sequence(harness.server(), responses).await;

    let test = harness.test();
    let codex = test.codex.clone();
    let session_model = test.session_configured.model.clone();
    let cwd = test.config.cwd.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.as_path());
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.into(),
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

    wait_for_event_match(&codex, |event| match event {
        EventMsg::ExecCommandBegin(begin) if begin.call_id == call_id => Some(()),
        _ => None,
    })
    .await;
    let end = wait_for_event_match(&codex, |event| match event {
        EventMsg::ExecCommandEnd(end) if end.call_id == call_id => Some(end.clone()),
        _ => None,
    })
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(end)
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_unified_exec_uses_shell_snapshot() -> Result<()> {
    let builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::ShellSnapshot)
            .expect("test config should allow feature update");
    });
    let harness = TestCodexHarness::with_builder(builder).await?;
    let codex_home = harness.test().home.path().to_path_buf();
    run_tool_turn_on_harness(
        &harness,
        "warm up PowerShell snapshot",
        "powershell-snapshot-warmup",
        json!({
            "cmd": "Microsoft.PowerShell.Utility\\Write-Output warmup",
            "yield_time_ms": 1_000,
        }),
    )
    .await?;
    let snapshot_path = wait_for_snapshot(&codex_home).await?;
    let snapshot_content = fs::read_to_string(&snapshot_path).await?;

    assert!(snapshot_path.starts_with(&codex_home));
    for section in ["# Snapshot file", "# Functions", "# aliases", "# exports"] {
        assert!(
            snapshot_content.lines().any(|line| line == section),
            "snapshot should contain exact section header {section:?}; snapshot={snapshot_content:?}"
        );
    }
    assert!(
        snapshot_content
            .lines()
            .any(|line| line == "# Codex PowerShell snapshot format: 1")
    );

    fs::write(
        &snapshot_path,
        "# Snapshot file\n# Codex PowerShell snapshot format: 1\n# Functions\nfunction Invoke-CodexSnapshotE2E { Microsoft.PowerShell.Utility\\Write-Output 'snapshot-windows' }\n# aliases\n# exports\n",
    )
    .await?;
    let end = run_tool_turn_on_harness(
        &harness,
        "verify PowerShell snapshot replay",
        "powershell-snapshot-replay",
        json!({
            "cmd": "Invoke-CodexSnapshotE2E",
            "yield_time_ms": 1_000,
        }),
    )
    .await?;

    assert_eq!(normalize_newlines(&end.stdout).trim(), "snapshot-windows");
    assert_eq!(end.exit_code, 0);

    Ok(())
}
