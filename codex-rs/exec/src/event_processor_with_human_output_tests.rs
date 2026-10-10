use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use codex_core::config::ConfigBuilder;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use codex_utils_sandbox_summary::summarize_permission_profile;
use owo_colors::Style;
use pretty_assertions::assert_eq;

use super::EventProcessorWithHumanOutput;
use super::config_summary_entries;
use super::reasoning_text;
use super::should_print_final_message_to_stdout;
use super::should_print_final_message_to_tty;
use super::write_final_message;
use crate::event_processor::EventProcessor;

#[test]
fn blended_token_total_is_nonnegative_and_saturates_at_the_display_limit() {
    for (input, cached, output, expected) in [
        (10, 3, 5, 12),
        (2, 5, 3, 3),
        (-1, -2, -3, 0),
        (i64::MIN, i64::MAX, 7, 7),
        (i64::MAX, 0, 1, i64::MAX),
        (i64::MAX, i64::MAX, i64::MAX, i64::MAX),
    ] {
        let total = codex_app_server_protocol::TokenUsageBreakdown {
            total_tokens: 0,
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            reasoning_output_tokens: 0,
        };
        let usage = codex_app_server_protocol::ThreadTokenUsage {
            last: total.clone(),
            total,
            model_context_window: None,
        };
        assert_eq!(super::blended_total(&usage), expected, "{input}/{cached}/{output}");
    }
}

#[test]
fn final_message_write_reports_closed_stdout_instead_of_panicking() {
    struct ClosedPipe;
    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut written = Vec::new();
    write_final_message(&mut written, "final answer").expect("write final message");
    assert_eq!(written, b"final answer\n");

    let error =
        write_final_message(&mut ClosedPipe, "final answer").expect_err("closed stdout must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert!(
        error
            .to_string()
            .contains("failed to write final message to stdout"),
        "{error}"
    );
}

#[test]
fn final_message_destinations_respect_terminal_and_rendered_state() {
    for (stdout_tty, stderr_tty, expected_stdout, expected_tty) in [
        (false, false, true, false),
        (false, true, true, false),
        (true, false, true, false),
        (true, true, false, true),
    ] {
        assert_eq!(should_print_final_message_to_stdout(Some("hello"), stdout_tty, stderr_tty), expected_stdout);
        assert!(!should_print_final_message_to_stdout(None, stdout_tty, stderr_tty));
        for rendered in [false, true] {
            assert_eq!(should_print_final_message_to_tty(Some("hello"), rendered, stdout_tty, stderr_tty), expected_tty && !rendered);
            assert!(!should_print_final_message_to_tty(None, rendered, stdout_tty, stderr_tty));
        }
    }
}

#[test]
fn reasoning_text_selects_and_joins_available_content() {
    for (summary, content, raw, expected) in [
        (vec!["summary"], vec!["raw"], false, Some("summary")),
        (vec!["summary"], vec!["raw"], true, Some("raw")),
        (vec!["one", "two"], vec![], true, Some("one\ntwo")),
        (vec![], vec!["one", "two"], true, Some("one\ntwo")),
        (vec![], vec!["raw"], false, None),
        (vec![], vec![], true, None),
    ] {
        let summary = summary.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let content = content.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(reasoning_text(&summary, &content, raw).as_deref(), expected);
    }
}

#[test]
fn summarizes_disabled_permission_profile_as_danger_full_access() {
    let cwd = test_path_buf("/tmp").abs();

    assert_eq!(
        summarize_permission_profile(
            &PermissionProfile::Disabled,
            &cwd,
            std::slice::from_ref(&cwd),
        ),
        "danger-full-access"
    );
}

#[test]
fn summarizes_external_permission_profile() {
    let cwd = test_path_buf("/tmp").abs();

    assert_eq!(
        summarize_permission_profile(
            &PermissionProfile::External {
                network: NetworkSandboxPolicy::Enabled,
            },
            &cwd,
            std::slice::from_ref(&cwd),
        ),
        "external-sandbox (network access enabled)"
    );
}

#[test]
fn summarizes_managed_workspace_write_permission_profile() {
    let cwd = test_path_buf("/tmp/project").abs();
    let cache_root = test_path_buf("/tmp/cache").abs();
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Path { path: cwd.clone() },
                access: FileSystemAccessMode::Write,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: cache_root.clone(),
                },
                access: FileSystemAccessMode::Write,
            },
        ]),
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        summarize_permission_profile(&profile, &cwd, &[cwd.clone(), cache_root.clone()]),
        format!("workspace-write [workdir, {}]", cache_root.display())
    );
}

#[test]
fn summarizes_managed_read_only_permission_profile() {
    let cwd = test_path_buf("/tmp/project").abs();
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(Vec::new()),
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        summarize_permission_profile(&profile, &cwd, std::slice::from_ref(&cwd)),
        "read-only"
    );
}

#[tokio::test]
async fn config_summary_entries_include_runtime_workspace_roots() {
    let codex_home = tempfile::tempdir().expect("create codex home");
    let cwd = tempfile::tempdir().expect("create cwd");
    let extra_root = tempfile::tempdir().expect("create extra root");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    let cwd = cwd.path().to_path_buf().abs();
    let extra_root = extra_root.path().to_path_buf().abs();
    let expected_extra_root_name = extra_root
        .file_name()
        .expect("extra root should have file name")
        .to_string_lossy()
        .to_string();
    config.cwd = cwd.clone();
    config.workspace_roots = vec![cwd.clone(), extra_root];
    config
        .permissions
        .set_workspace_roots(config.workspace_roots.clone());
    config
        .permissions
        .set_permission_profile(PermissionProfile::workspace_write_with(
            &[],
            NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        ))
        .expect("set permission profile");

    let session_configured_event = SessionConfiguredEvent {
        session_id: SessionId::new(),
        thread_id: ThreadId::new(),
        forked_from_id: None,
        parent_thread_id: None,
        thread_source: None,
        thread_name: None,
        model: "gpt-5.4".to_string(),
        model_provider_id: config.model_provider_id.clone(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: config.permissions.effective_permission_profile(),
        active_permission_profile: None,
        cwd,
        reasoning_effort: None,
        initial_messages: None,
        network_proxy: None,
        rollout_path: None,
    };

    let summary_entries = config_summary_entries(&config, &session_configured_event);
    let sandbox_summary = summary_entries
        .iter()
        .find_map(|(key, value)| (*key == "sandbox").then_some(value))
        .expect("sandbox summary entry");
    assert!(
        sandbox_summary.starts_with("workspace-write [workdir, ")
            && sandbox_summary.contains(&expected_extra_root_name),
        "expected runtime workspace root in sandbox summary: {summary_entries:?}"
    );
}



#[test]
fn turn_completed_overwrites_stale_final_message_from_turn_items() {
    for (previous, rendered, expected_rendered) in [
        (None, false, false),
        (Some("stale answer"), true, false),
        (Some("final answer"), true, true),
    ] {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: previous.map(str::to_owned),
        final_message_rendered: rendered,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            surfaced_result: None,
            thread_id: "thread-1".to_string(),
            timing: None,
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![ThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "final answer".to_string(),
                    phase: None,
                }],
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("final answer"));
    assert_eq!(processor.final_message_rendered, expected_rendered);
    assert!(processor.emit_final_message_on_shutdown);
    }
}

#[test]
fn turn_completed_preserves_streamed_final_message_when_turn_items_are_empty() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: Some("streamed answer".to_string()),
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            surfaced_result: None,
            thread_id: "thread-1".to_string(),
            timing: None,
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("streamed answer"));
    assert!(processor.emit_final_message_on_shutdown);
}

#[test]
fn turn_failed_clears_stale_final_message() {
    for turn_status in [TurnStatus::Failed, TurnStatus::Interrupted] {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        last_message_path: None,
        final_message: Some("partial answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: true,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            surfaced_result: None,
            thread_id: "thread-1".to_string(),
            timing: None,
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: turn_status,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message, None);
    assert!(!processor.final_message_rendered);
    assert!(!processor.emit_final_message_on_shutdown);
    }
}



#[test]
fn canonical_message_retains_rendered_state_only_when_unchanged() {
    for (canonical, expected_rendered) in [("streamed answer", true), ("updated answer", false)] {
        let mut processor = EventProcessorWithHumanOutput {
            bold: Style::new(),
            cyan: Style::new(),
            dimmed: Style::new(),
            green: Style::new(),
            italic: Style::new(),
            magenta: Style::new(),
            red: Style::new(),
            yellow: Style::new(),
            show_agent_reasoning: true,
            show_raw_agent_reasoning: false,
            last_message_path: None,
            final_message: None,
            final_message_rendered: false,
            emit_final_message_on_shutdown: false,
            last_total_token_usage: None,
        };
        processor.process_server_notification(ServerNotification::ItemCompleted(
            codex_app_server_protocol::ItemCompletedNotification {
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                completed_at_ms: 0,
                item: ThreadItem::AgentMessage {
                    id: "message".into(),
                    text: "streamed answer".into(),
                    phase: None,
                },
            },
        ));
        assert!(processor.final_message_rendered);
        let ServerNotification::TurnCompleted(mut payload) = crate::tests::recovery_completion()
        else {
            panic!("completion")
        };
        payload.surfaced_result = Some(codex_protocol::protocol::SurfacedToolResult {
            adapter: "owner".into(),
            value: serde_json::json!({}),
            canonical_message: Some(canonical.into()),
        });
        processor.process_server_notification(ServerNotification::TurnCompleted(payload));
        assert_eq!(processor.final_message.as_deref(), Some(canonical));
        assert_eq!(processor.final_message_rendered, expected_rendered);
        assert!(processor.emit_final_message_on_shutdown);
        processor.process_event_stream_error("lost stream".into());
        assert_eq!(processor.final_message, None);
        assert!(!processor.final_message_rendered);
        assert!(!processor.emit_final_message_on_shutdown);
    }
}
