use anyhow::Result;
use codex_core::config::Constrained;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use core_test_support::TempDirExt;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
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

fn persisted_thread_settings(path: &std::path::Path) -> Result<Vec<ThreadSettingsSnapshot>> {
    let rollout = std::fs::read_to_string(path)?;
    let mut settings = Vec::new();
    for line in rollout.lines().filter(|line| !line.trim().is_empty()) {
        let rollout_line: RolloutLine = serde_json::from_str(line)?;
        if let RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) = rollout_line.item {
            settings.push(event.thread_settings);
        }
    }
    Ok(settings)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_settings_update_without_user_turn_records_permissions_update() -> Result<()> {
    skip_if_no_network!(Ok(()));

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

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let settings = persisted_thread_settings(&rollout_path)?;
    assert_eq!(settings.len(), 1);
    assert_eq!(settings[0].approval_policy, AskForApproval::Never);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_settings_update_without_user_turn_records_environment_update() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build(&server).await?;
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

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let settings = persisted_thread_settings(&rollout_path)?;
    assert_eq!(settings.len(), 1);
    assert_eq!(settings[0].environments.as_ref(), Some(&environments));
    assert_eq!(settings[0].cwd, new_cwd.abs());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_settings_update_without_user_turn_records_collaboration_update() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build(&server).await?;
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
    let settings = persisted_thread_settings(&rollout_path)?;
    assert_eq!(settings.len(), 1);
    assert_eq!(settings[0].collaboration_mode, collaboration_mode);

    Ok(())
}
