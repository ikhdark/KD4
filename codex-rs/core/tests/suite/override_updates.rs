use anyhow::Result;
use codex_core::config::Constrained;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use core_test_support::TempDirExt;
use core_test_support::require_network;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use tempfile::TempDir;

fn collab_mode_with_instructions(instructions: Option<&str>) -> CollaborationMode {
    CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model: "gpt-5.4".to_string(),
            reasoning_effort: None,
            developer_instructions: instructions.map(str::to_string),
        },
    }
}

async fn persisted_thread_settings(path: &std::path::Path) -> Result<Vec<ThreadSettingsSnapshot>> {
    let (items, _, parse_errors) = codex_rollout::RolloutRecorder::load_rollout_items(path).await?;
    anyhow::ensure!(parse_errors == 0, "invalid rollout records");
    let mut settings = Vec::new();
    for item in items {
        if let RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) = item {
            settings.push(event.thread_settings);
        }
    }
    Ok(settings)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_settings_updates_without_user_turn_persist_cumulative_snapshots() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    });
    let test = builder.build(&server).await?;

    core_test_support::submit_thread_settings(
        &test.codex,
        codex_protocol::protocol::ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            ..Default::default()
        },
    )
    .await?;

    let new_cwd = TempDir::new()?;
    let environments = local_selections(new_cwd.abs());

    core_test_support::submit_thread_settings(
        &test.codex,
        codex_protocol::protocol::ThreadSettingsOverrides {
            environments: Some(environments.clone()),
            ..Default::default()
        },
    )
    .await?;

    let collab_text = "override collaboration instructions";
    let collaboration_mode = collab_mode_with_instructions(Some(collab_text));

    core_test_support::submit_thread_settings(
        &test.codex,
        codex_protocol::protocol::ThreadSettingsOverrides {
            collaboration_mode: Some(collaboration_mode.clone()),
            ..Default::default()
        },
    )
    .await?;

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let settings = persisted_thread_settings(&rollout_path).await?;
    assert_eq!(settings.len(), 3);
    assert_eq!(settings[0].approval_policy, AskForApproval::Never);
    assert_eq!(settings[0].cwd, test.config.cwd);
    assert_eq!(settings[1].approval_policy, AskForApproval::Never);
    assert_eq!(settings[1].environments.as_ref(), Some(&environments));
    assert_eq!(settings[1].cwd, new_cwd.abs());
    assert_eq!(settings[2].approval_policy, AskForApproval::Never);
    assert_eq!(settings[2].environments.as_ref(), Some(&environments));
    assert_eq!(settings[2].cwd, new_cwd.abs());
    assert_eq!(settings[2].collaboration_mode, collaboration_mode);

    Ok(())
}
