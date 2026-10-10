use super::*;
use crate::app_event::HistoryLookupResponse;
use codex_app_server_protocol::NetworkAccess;
use codex_app_server_protocol::SandboxPolicy;
use codex_protocol::models::ManagedFileSystemPermissions;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn completed_file_changes_are_visible_in_replayed_turns() {
    for replay_kind in [
        ReplayKind::ResumeInitialMessages,
        ReplayKind::ThreadSnapshot,
    ] {
        let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
        chat.show_welcome_banner = false;
        let _ = drain_insert_history(&mut rx);
        let item = serde_json::from_value(json!({
            "type": "fileChange", "id": "patch-1", "status": "completed",
            "changes": [{"path": "replayed.txt", "kind": {"type": "add"}, "diff": "restored content\n"}]
        })).unwrap();
        chat.replay_thread_turns(
            vec![AppServerTurn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![item],
                status: AppServerTurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            }],
            replay_kind,
        );
        let mut transcript = String::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::InsertHistoryCell(cell) => {
                    transcript.push_str(&lines_to_single_string(&cell.display_lines(80)))
                }
                event => assert!(!matches!(event, AppEvent::SubmitThreadOp { .. })),
            }
        }
        assert_eq!(
            transcript.matches("replayed.txt").count(),
            1,
            "{transcript}"
        );
        assert!(transcript.contains("restored content"), "{transcript}");
        assert!(op_rx.try_recv().is_err());
    }
}

#[tokio::test]
async fn dynamic_tool_results_render_through_live_notifications_and_resumed_turns() {
    for replay_kind in [
        None,
        Some(ReplayKind::ResumeInitialMessages),
        Some(ReplayKind::ThreadSnapshot),
    ] {
        let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
        chat.show_welcome_banner = false;
        let _ = drain_insert_history(&mut rx);
        let items: Vec<AppServerThreadItem> = serde_json::from_value(json!([
            {
                "type": "dynamicToolCall", "id": "lookup-1", "namespace": "catalog",
                "tool": "lookup", "arguments": {"sku": "123"}, "status": "completed",
                "contentItems": [
                    {"type": "inputText", "text": "Found item\nAvailable: 2"},
                    {"type": "inputImage", "imageUrl": "data:image/png;base64,not-transcript-text"},
                    {"type": "inputText", "text": "End of result"}
                ], "success": true, "error": null, "durationMs": 5
            },
            {
                "type": "dynamicToolCall", "id": "lookup-2", "namespace": null,
                "tool": "lookup", "arguments": {}, "status": "failed",
                "contentItems": [{"type": "inputText", "text": "Partial result"}],
                "success": false, "error": "Catalog unavailable", "durationMs": 2
            },
            {
                "type": "dynamicToolCall", "id": "lookup-3", "namespace": null,
                "tool": "legacy_lookup", "arguments": {}, "status": "completed",
                "contentItems": null, "success": false, "error": "Request rejected",
                "durationMs": null
            }
        ]))
        .expect("normal app-server dynamic tool items");

        if let Some(replay_kind) = replay_kind {
            chat.replay_thread_turns(
                vec![AppServerTurn {
                    id: "turn-1".to_string(),
                    items_view: codex_app_server_protocol::TurnItemsView::Full,
                    items,
                    status: AppServerTurnStatus::Completed,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    timing: None,
                    surfaced_result: None,
                }],
                replay_kind,
            );
        } else {
            for item in items {
                let mut started = item.clone();
                let AppServerThreadItem::DynamicToolCall {
                    status,
                    content_items,
                    success,
                    error,
                    ..
                } = &mut started
                else {
                    unreachable!()
                };
                *status = codex_app_server_protocol::DynamicToolCallStatus::InProgress;
                *content_items = None;
                *success = None;
                *error = None;
                chat.handle_server_notification(
                    ServerNotification::ItemStarted(ItemStartedNotification {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        started_at_ms: 0,
                        item: started,
                    }),
                    None,
                );
                chat.handle_server_notification(
                    ServerNotification::ItemCompleted(ItemCompletedNotification {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        completed_at_ms: 5,
                        item,
                    }),
                    None,
                );
            }
        }

        let cells = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(cell),
                other => {
                    assert!(
                        !matches!(other, AppEvent::SubmitThreadOp { .. }),
                        "rendering must not execute a tool"
                    );
                    None
                }
            })
            .collect::<Vec<_>>();
        let expected = [
            "• Called catalog.lookup\n  Found item\n  Available: 2\n  [Image output]\n  End of result\n",
            "• Failed lookup\n  Partial result\n  Error: Catalog unavailable\n",
            "• Failed legacy_lookup\n  Error: Request rejected\n",
        ];
        assert_eq!(
            cells.len(),
            expected.len(),
            "one terminal result per tool call"
        );
        for (cell, expected) in cells.iter().zip(expected) {
            assert_eq!(lines_to_single_string(&cell.display_lines(80)), expected);
            assert_eq!(lines_to_single_string(&cell.transcript_lines(80)), expected);
            let narrow = cell.display_lines(20);
            assert!(narrow.iter().all(|line| line.width() <= 20));
            assert_eq!(
                lines_to_single_string(&narrow)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
                expected
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
                "narrow terminals must wrap the output without losing text"
            );
        }
        assert!(
            op_rx.try_recv().is_err(),
            "live and replay rendering must not execute tools"
        );
    }
}

#[tokio::test]
async fn resumed_initial_messages_render_history() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;

    let thread_id = ThreadId::new();
    let rollout_file = NamedTempFile::new().unwrap();
    let configured = crate::session_state::ThreadSessionState {
        thread_id,
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: PermissionProfile::read_only(),
        active_permission_profile: None,
        cwd: test_path_buf("/home/user/project").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: Some(ReasoningEffortConfig::default()),
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: Some(rollout_file.path().to_path_buf()),
    };

    chat.handle_thread_session(configured);
    replay_user_message_text(
        &mut chat,
        "user-1",
        "hello from user",
        ReplayKind::ResumeInitialMessages,
    );
    replay_agent_message(
        &mut chat,
        "assistant-1",
        "assistant reply",
        ReplayKind::ResumeInitialMessages,
    );

    let cells = drain_insert_history(&mut rx);
    let mut merged_lines = Vec::new();
    for lines in cells {
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.clone())
            .collect::<String>();
        merged_lines.push(text);
    }

    let text_blob = merged_lines.join("\n");
    assert!(
        text_blob.contains("hello from user"),
        "expected replayed user message",
    );
    assert!(
        text_blob.contains("assistant reply"),
        "expected replayed agent message",
    );
}

#[tokio::test]
async fn replayed_user_messages_seed_composer_history() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.bottom_pane.set_history_metadata(
        ThreadId::new(),
        /*log_id*/ 1,
        /*entry_count*/ 3,
    );

    let mut replay_mention = |id: &str, text: &str, name: &str, path: &str| {
        replay_user_message_inputs(
            &mut chat,
            id,
            vec![
                AppServerUserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                },
                AppServerUserInput::Mention {
                    name: name.to_string(),
                    path: path.to_string(),
                },
            ],
            ReplayKind::ResumeInitialMessages,
        );
    };
    replay_mention(
        "user-1",
        "use $sample",
        "Sample Plugin",
        "plugin://sample@test",
    );
    replay_mention(
        "user-2",
        "use $google-calendar",
        "Google Calendar",
        "app://google_calendar",
    );
    drain_insert_history(&mut rx);

    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(chat.bottom_pane.composer_text(), "use $google-calendar");
    assert_eq!(
        chat.bottom_pane.take_mention_bindings(),
        vec![MentionBinding {
            sigil: '$',
            mention: "google-calendar".to_string(),
            path: "app://google_calendar".to_string(),
        }]
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(chat.bottom_pane.composer_text(), "use $sample");
    assert_eq!(
        chat.bottom_pane.take_mention_bindings(),
        vec![MentionBinding {
            sigil: '$',
            mention: "sample".to_string(),
            path: "plugin://sample@test".to_string(),
        }]
    );

    let mut next_lookup_offset = || {
        let AppEvent::LookupMessageHistoryEntry { offset, .. } =
            rx.try_recv().expect("expected lookup")
        else {
            panic!("unexpected event variant");
        };
        offset
    };
    let response = |offset, entry: &str| HistoryLookupResponse {
        offset,
        log_id: 1,
        entry: Some(entry.to_string()),
    };

    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    chat.handle_history_entry_response(response(
        next_lookup_offset(),
        "use [$google-calendar](app://google_calendar)",
    ));

    assert_eq!(next_lookup_offset(), 1);
    chat.handle_history_entry_response(response(1, "use [$sample](plugin://sample@test)"));

    assert_eq!(next_lookup_offset(), 0);
    chat.handle_history_entry_response(response(0, "/rename smoke-1"));
    assert_eq!(chat.bottom_pane.composer_text(), "/rename smoke-1");
}

#[tokio::test]
async fn replayed_review_prompt_does_not_seed_composer_history() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.replay_thread_item(
        AppServerThreadItem::EnteredReviewMode {
            id: "review-start".to_string(),
            review: "changes against main".to_string(),
        },
        "turn-1".to_string(),
        ReplayKind::ResumeInitialMessages,
    );
    replay_user_message_text(
        &mut chat,
        "review-prompt",
        "Review the code changes against the base branch 'main'.",
        ReplayKind::ResumeInitialMessages,
    );
    drain_insert_history(&mut rx);

    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(chat.bottom_pane.composer_text(), "");
}

#[tokio::test]
async fn replayed_user_messages_preserve_text_elements_and_local_or_remote_images() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    let thread_id = ThreadId::new();
    let rollout_file = NamedTempFile::new().unwrap();
    let configured = crate::session_state::ThreadSessionState {
        thread_id,
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: PermissionProfile::read_only(),
        active_permission_profile: None,
        cwd: test_path_buf("/home/user/project").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: Some(ReasoningEffortConfig::default()),
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: Some(rollout_file.path().to_path_buf()),
    };

    chat.handle_thread_session(configured);

    drain_insert_history(&mut rx);
    for remote in [false, true] {
        for with_text in [false, true] {
            let placeholder = "[Image #1]";
            let message = if with_text { format!("{placeholder} replayed") } else { String::new() };
            let elements = if with_text {
                vec![TextElement::new((0..placeholder.len()).into(), Some(placeholder.to_string()))]
            } else { Vec::new() };
            let local_images = if remote { Vec::new() } else { vec![PathBuf::from("/tmp/replay.png")] };
            let remote_urls = if remote { vec!["https://example.com/image.png".to_string()] } else { Vec::new() };
            let mut inputs = Vec::new();
            if with_text {
                inputs.push(AppServerUserInput::Text {
                    text: message.clone(),
                    text_elements: elements.clone().into_iter().map(Into::into).collect(),
                });
            }
            inputs.push(if remote {
                AppServerUserInput::Image { url: remote_urls[0].clone(), detail: None }
            } else {
                AppServerUserInput::LocalImage { path: local_images[0].clone(), detail: None }
            });
            replay_user_message_inputs(&mut chat, &format!("user-{remote}-{with_text}"), inputs, ReplayKind::ResumeInitialMessages);
            let cells = std::iter::from_fn(|| rx.try_recv().ok()).filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(cell),
                _ => None,
            }).collect::<Vec<_>>();
            assert_eq!(cells.len(), 1);
            let cell = cells[0].as_any().downcast_ref::<UserHistoryCell>().expect("replayed user cell");
            assert_eq!(cell.message, message);
            assert_eq!(cell.text_elements, elements);
            assert_eq!(cell.local_image_paths, local_images);
            assert_eq!(cell.remote_image_urls, remote_urls);
        }
    }
}



#[tokio::test]
async fn session_configured_syncs_widget_config_permissions_and_cwd() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest.to_core())
        .expect("set approval policy");
    chat.config
        .permissions
        .set_permission_profile(PermissionProfile::workspace_write())
        .expect("set permission profile");
    chat.config.cwd = test_path_buf("/home/user/main").abs();

    let expected_cwd = test_path_buf("/home/user/sub-agent").abs();
    let expected_app_server_permission_profile = PermissionProfile::Managed {
        network: NetworkSandboxPolicy::Restricted,
        file_system: ManagedFileSystemPermissions::Restricted {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    access: FileSystemAccessMode::Read,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: "**/.secret".to_string(),
                    },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: None,
        },
    };
    let expected_permission_profile = expected_app_server_permission_profile.clone();
    let expected_core_sandbox = expected_permission_profile
        .to_legacy_sandbox_policy(expected_cwd.as_path())
        .expect("permission profile should project to legacy sandbox policy");
    let expected_sandbox = SandboxPolicy::from(expected_core_sandbox);
    let configured = crate::session_state::ThreadSessionState {
        thread_id: ThreadId::new(),
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: expected_permission_profile,
        active_permission_profile: None,
        cwd: expected_cwd.clone(),
        runtime_workspace_roots: vec![expected_cwd.clone()],
        instruction_source_paths: Vec::new(),
        reasoning_effort: Some(ReasoningEffortConfig::default()),
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: None,
    };

    chat.handle_thread_session(configured);

    assert_eq!(
        AskForApproval::from(chat.config_ref().permissions.approval_policy.value()),
        AskForApproval::Never
    );
    let actual_sandbox = SandboxPolicy::from(chat.config_ref().legacy_sandbox_policy());
    assert_eq!(&actual_sandbox, &expected_sandbox);
    assert_eq!(
        chat.config_ref().permissions.effective_permission_profile(),
        expected_app_server_permission_profile
    );
    assert_eq!(&chat.config_ref().cwd, &expected_cwd);

    let updated_profile = PermissionProfile::workspace_write();
    chat.set_permission_profile_from_session_snapshot(PermissionProfileSnapshot::legacy(
        updated_profile.clone(),
    ))
    .expect("set permission profile");
    assert_eq!(
        chat.config_ref().permissions.permission_profile(),
        &updated_profile,
        "local permission changes should replace SessionConfigured canonical permissions"
    );
    assert_eq!(
        chat.config_ref().permissions.effective_permission_profile(),
        updated_profile
            .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&expected_cwd,)),
        "effective permissions should still use the current thread runtime workspace roots"
    );
}

#[tokio::test]
async fn session_configured_preserves_profile_workspace_roots() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;

    let previous_cwd = test_path_buf("/home/user/main").abs();
    let profile_root = test_path_buf("/home/user/shared").abs();
    chat.config.cwd = previous_cwd.clone();
    chat.config.workspace_roots = vec![previous_cwd, profile_root.clone()];
    chat.config
        .permissions
        .set_workspace_roots(chat.config.workspace_roots.clone());

    let session_cwd = test_path_buf("/home/user/sub-agent").abs();
    let session_runtime_workspace_roots = vec![session_cwd.clone()];
    let session_effective_workspace_roots = vec![session_cwd.clone(), profile_root];
    let session_permission_profile = PermissionProfile::workspace_write()
        .materialize_project_roots_with_workspace_roots(&session_effective_workspace_roots);
    let configured = crate::session_state::ThreadSessionState {
        thread_id: ThreadId::new(),
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: session_permission_profile.clone(),
        active_permission_profile: None,
        cwd: session_cwd.clone(),
        runtime_workspace_roots: session_runtime_workspace_roots.clone(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: Some(ReasoningEffortConfig::default()),
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: None,
    };

    chat.handle_thread_session(configured);

    assert_eq!(&chat.config_ref().cwd, &session_cwd);
    assert_eq!(
        chat.config_ref().permissions.user_visible_workspace_roots(),
        session_runtime_workspace_roots.as_slice()
    );
    assert_eq!(
        chat.config_ref().permissions.effective_permission_profile(),
        session_permission_profile
    );
}

#[tokio::test]
async fn session_configured_external_sandbox_keeps_external_runtime_policy() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;

    let expected_app_server_permission_profile = PermissionProfile::External {
        network: NetworkSandboxPolicy::Restricted,
    };
    let expected_permission_profile = expected_app_server_permission_profile.clone();
    let expected_sandbox = SandboxPolicy::ExternalSandbox {
        network_access: NetworkAccess::Restricted,
    };
    let configured = crate::session_state::ThreadSessionState {
        thread_id: ThreadId::new(),
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: expected_permission_profile,
        active_permission_profile: None,
        cwd: test_path_buf("/home/user/external").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: Some(ReasoningEffortConfig::default()),
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: None,
    };

    chat.handle_thread_session(configured);

    let actual_sandbox = SandboxPolicy::from(chat.config_ref().legacy_sandbox_policy());
    assert_eq!(&actual_sandbox, &expected_sandbox);
    assert_eq!(
        chat.config_ref().permissions.effective_permission_profile(),
        expected_app_server_permission_profile
    );
}





#[tokio::test]
async fn forked_thread_history_uses_supplied_title_and_ignores_local_index() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(None).await;
    let temp = tempdir().expect("tempdir");
    chat.config.codex_home = AbsolutePathBuf::from_absolute_path(temp.path()).expect("absolute home");
    for (name, id, local_name, snapshot) in [
        (Some("named-thread"), "e9f18a88-8081-4e51-9d4e-8af5cde2d8dd", None, "forked_thread_history_line"),
        (None, "019c2d47-4935-7423-a190-05691f566092", None, "forked_thread_history_line_without_name"),
        (Some("app-server-parent-thread"), "e9f18a88-8081-4e51-9d4e-8af5cde2d8dd", Some("stale-local-thread"), "app_server_forked_thread_history_line"),
        (None, "019c2d47-4935-7423-a190-05691f566092", Some("stale-local-thread"), "app_server_forked_thread_history_line_without_app_server_name"),
    ] {
        let forked_from_id = ThreadId::from_string(id).expect("forked id");
        if let Some(local_name) = local_name {
            std::fs::write(temp.path().join("session_index.jsonl"), format!(
                "{{\"id\":\"{id}\",\"thread_name\":\"{local_name}\",\"updated_at\":\"2024-01-02T00:00:00Z\"}}\n"
            )).expect("write session index");
        }
        chat.emit_forked_thread_event(forked_from_id, name.map(str::to_string));
        let cells = drain_insert_history(&mut rx);
        assert_eq!(cells.len(), 1);
        let combined = lines_to_single_string(&cells[0]);
        assert!(combined.contains("Thread forked from"));
        assert_eq!(combined.matches(id).count(), 1);
        if let Some(name) = name { assert!(combined.contains(name)); }
        assert!(!combined.contains("stale-local-thread"));
        assert_chatwidget_snapshot!(snapshot, combined);
    }
}







#[tokio::test]
async fn thread_snapshot_replay_preserves_agent_message_during_review_mode() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;

    replay_entered_review_mode(&mut chat, "current changes");
    let _ = drain_insert_history(&mut rx);

    replay_agent_message(
        &mut chat,
        "review-message",
        "Review progress update",
        ReplayKind::ThreadSnapshot,
    );

    let inserted = drain_insert_history(&mut rx);
    assert_eq!(inserted.len(), 1);
    assert!(lines_to_single_string(&inserted[0]).contains("Review progress update"));
}

#[tokio::test]
async fn replayed_retryable_app_server_error_keeps_turn_running() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.handle_server_notification(
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: "thread-1".to_string(),
            turn: AppServerTurn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: AppServerTurnStatus::InProgress,
                error: None,
                started_at: Some(0),
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        }),
        Some(ReplayKind::ThreadSnapshot),
    );
    drain_insert_history(&mut rx);

    chat.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            error: AppServerTurnError {
                message: "Reconnecting... 1/5".to_string(),
                codex_error_info: None,
                additional_details: Some("Idle timeout waiting for SSE".to_string()),
            },
            will_retry: true,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
        }),
        Some(ReplayKind::ThreadSnapshot),
    );

    assert!(drain_insert_history(&mut rx).is_empty());
    assert!(chat.bottom_pane.is_task_running());
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Working");
    assert_eq!(status.details(), None);
}

#[tokio::test]
async fn replayed_thread_closed_notification_does_not_exit_tui() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.handle_server_notification(
        ServerNotification::ThreadClosed(ThreadClosedNotification {
            thread_id: "thread-1".to_string(),
        }),
        Some(ReplayKind::ThreadSnapshot),
    );

    assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn replayed_reasoning_item_preserves_summary_parts_and_hides_raw_reasoning_when_disabled() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config.show_raw_agent_reasoning = false;
    chat.handle_thread_session(crate::session_state::ThreadSessionState {
        thread_id: ThreadId::new(),
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: PermissionProfile::read_only(),
        active_permission_profile: None,
        cwd: test_project_path().abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: None,
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: None,
    });
    let _ = drain_insert_history(&mut rx);

    chat.replay_thread_item(
        AppServerThreadItem::Reasoning {
            id: "reasoning-1".to_string(),
            summary: vec![
                "**Plan**\n\ndone".to_string(),
                "**Checking tests**\n\n<!-- -->".to_string(),
            ],
            content: vec!["Raw reasoning".to_string()],
        },
        "turn-1".to_string(),
        ReplayKind::ThreadSnapshot,
    );

    let rendered = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            lines_to_single_string(&cell.transcript_lines(/*width*/ 80))
        }
        other => panic!("expected InsertHistoryCell, got {other:?}"),
    };
    assert_eq!(rendered, "• done\n");
    assert!(!rendered.contains("Raw reasoning"));
}

#[tokio::test]
async fn replayed_reasoning_item_shows_raw_reasoning_when_enabled() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config.show_raw_agent_reasoning = true;
    chat.handle_thread_session(crate::session_state::ThreadSessionState {
        thread_id: ThreadId::new(),
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: PermissionProfile::read_only(),
        active_permission_profile: None,
        cwd: test_project_path().abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: None,
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: None,
    });
    let _ = drain_insert_history(&mut rx);

    chat.replay_thread_item(
        AppServerThreadItem::Reasoning {
            id: "reasoning-1".to_string(),
            summary: vec!["Summary only".to_string()],
            content: vec!["Raw reasoning".to_string()],
        },
        "turn-1".to_string(),
        ReplayKind::ThreadSnapshot,
    );

    let rendered = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            lines_to_single_string(&cell.transcript_lines(/*width*/ 80))
        }
        other => panic!("expected InsertHistoryCell, got {other:?}"),
    };
    assert!(rendered.contains("Raw reasoning"));
}

#[tokio::test]
async fn replayed_in_progress_mcp_tool_call_stays_active() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let _ = drain_insert_history(&mut rx);

    chat.replay_thread_item(
        AppServerThreadItem::McpToolCall {
            id: "mcp-1".to_string(),
            server: "copilot-bridge".to_string(),
            tool: "copilot".to_string(),
            status: codex_app_server_protocol::McpToolCallStatus::InProgress,
            arguments: json!({"action": "wait"}),
            app_context: None,
            mcp_app_resource_uri: None,
            plugin_id: None,
            result: None,
            error: None,
            duration_ms: None,
        },
        "turn-1".to_string(),
        ReplayKind::ThreadSnapshot,
    );

    assert!(drain_insert_history(&mut rx).is_empty());
    let active = active_blob(&chat);
    assert!(active.contains("Calling"));
    assert!(!active.contains("MCP tool call completed without a result"));
}

#[tokio::test]
async fn live_reasoning_summary_is_not_rendered_twice_when_item_completes() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.show_welcome_banner = false;

    chat.handle_server_notification(
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: "thread-1".to_string(),
            turn: AppServerTurn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: AppServerTurnStatus::InProgress,
                error: None,
                started_at: Some(0),
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        }),
        /*replay_kind*/ None,
    );
    let _ = drain_insert_history(&mut rx);

    chat.handle_server_notification(
        ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "reasoning-1".to_string(),
            delta: "Summary only".to_string(),
            summary_index: 0,
        }),
        /*replay_kind*/ None,
    );

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: AppServerThreadItem::Reasoning {
                id: "reasoning-1".to_string(),
                summary: vec!["Summary only".to_string()],
                content: Vec::new(),
            },
        }),
        /*replay_kind*/ None,
    );

    let rendered = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            lines_to_single_string(&cell.transcript_lines(/*width*/ 80))
        }
        other => panic!("expected InsertHistoryCell, got {other:?}"),
    };
    assert_eq!(rendered.matches("Summary only").count(), 1);
}

#[tokio::test]
async fn live_reasoning_summary_drops_empty_parts_without_losing_content() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.show_welcome_banner = false;

    chat.handle_server_notification(
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: "thread-1".to_string(),
            turn: AppServerTurn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: AppServerTurnStatus::InProgress,
                error: None,
                started_at: Some(0),
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            },
        }),
        /*replay_kind*/ None,
    );
    let _ = drain_insert_history(&mut rx);

    for (summary_index, delta) in [
        (0, "**Plan**\n\ndone"),
        (1, "**Checking tests**\n\n<!-- -->"),
    ] {
        chat.handle_server_notification(
            ServerNotification::ReasoningSummaryPartAdded(
                codex_app_server_protocol::ReasoningSummaryPartAddedNotification {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "reasoning-1".to_string(),
                    summary_index,
                },
            ),
            /*replay_kind*/ None,
        );
        chat.handle_server_notification(
            ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "reasoning-1".to_string(),
                delta: delta.to_string(),
                summary_index,
            }),
            /*replay_kind*/ None,
        );
    }

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: AppServerThreadItem::Reasoning {
                id: "reasoning-1".to_string(),
                summary: vec![
                    "**Plan**\n\ndone".to_string(),
                    "**Checking tests**\n\n<!-- -->".to_string(),
                ],
                content: Vec::new(),
            },
        }),
        /*replay_kind*/ None,
    );

    let rendered = match rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => {
            lines_to_single_string(&cell.transcript_lines(/*width*/ 80))
        }
        other => panic!("expected InsertHistoryCell, got {other:?}"),
    };
    assert_eq!(rendered, "• done\n");
}

#[tokio::test]
async fn thread_snapshot_replayed_turn_started_marks_task_running() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    replay_turn_started(&mut chat, ReplayKind::ThreadSnapshot);

    drain_insert_history(&mut rx);
    assert!(chat.bottom_pane.is_task_running());
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Working");
}

#[tokio::test]
async fn replayed_in_progress_turn_marks_task_running() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.replay_thread_turns(
        vec![AppServerTurn {
            id: "turn-1".to_string(),
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            items: Vec::new(),
            status: AppServerTurnStatus::InProgress,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            timing: None,
            surfaced_result: None,
        }],
        ReplayKind::ResumeInitialMessages,
    );

    assert!(drain_insert_history(&mut rx).is_empty());
    assert!(chat.bottom_pane.is_task_running());
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Working");
}

#[tokio::test]
async fn replayed_stream_error_does_not_set_retry_status_or_status_indicator() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_status_header("Idle".to_string());

    handle_stream_error_with_replay(
        &mut chat,
        "Reconnecting... 2/5",
        Some("Idle timeout waiting for SSE".to_string()),
        Some(ReplayKind::ResumeInitialMessages),
    );

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "expected no history cell for replayed StreamError event"
    );
    assert_eq!(chat.status_state.current_status.header, "Idle");
    assert!(chat.status_state.retry_status_header.is_none());
    assert!(chat.bottom_pane.status_widget().is_none());
}

#[tokio::test]
async fn thread_snapshot_stream_recovery_preserves_status_without_entering_retry() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    replay_turn_started(&mut chat, ReplayKind::ThreadSnapshot);
    drain_insert_history(&mut rx);

    handle_stream_error_with_replay(
        &mut chat,
        "Reconnecting... 1/5",
        /*additional_details*/ None,
        Some(ReplayKind::ThreadSnapshot),
    );
    drain_insert_history(&mut rx);
    // The delta below restores a remembered header, so check before it arrives.
    assert_eq!(chat.status_state.current_status.header, "Working");
    assert!(chat.status_state.retry_status_header.is_none());

    replay_agent_message_delta(&mut chat, "hello", ReplayKind::ThreadSnapshot);

    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Working");
    assert_eq!(status.details(), None);
    assert!(chat.status_state.retry_status_header.is_none());
}

#[tokio::test]
async fn stream_recovery_restores_previous_status_header() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");
    drain_insert_history(&mut rx);
    handle_stream_error(
        &mut chat,
        "Reconnecting... 1/5",
        /*additional_details*/ None,
    );
    drain_insert_history(&mut rx);
    assert_eq!(chat.bottom_pane.status_widget().expect("retry status").header(), "Reconnecting... 1/5");
    assert!(chat.status_state.retry_status_header.is_some());
    handle_agent_message_delta(&mut chat, "hello");

    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Working");
    assert_eq!(status.details(), None);
    assert!(chat.status_state.retry_status_header.is_none());
}
