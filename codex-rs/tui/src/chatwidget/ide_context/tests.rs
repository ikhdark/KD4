use super::*;
use crate::app_command::AppCommand;
use crate::bottom_pane::ComposerDraftSnapshot;
use crate::bottom_pane::MentionBinding;
use crate::chatwidget::UserMessageHistoryOverride;
use crate::chatwidget::tests::helpers::make_chatwidget_manual;
use codex_utils_absolute_path::AbsolutePathBuf;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

fn context(path: &str) -> IdeContext {
    serde_json::from_value(serde_json::json!({
        "openTabs": [{ "label": path, "path": path }]
    }))
    .expect("IDE context")
}

fn pending_id(chat: &ChatWidget) -> Uuid {
    chat.ide_context
        .prompt_request
        .as_ref()
        .expect("pending IDE prompt")
        .id
}

fn next_turn(rx: &mut UnboundedReceiver<AppCommand>) -> Vec<UserInput> {
    loop {
        if let AppCommand::UserTurn { items, .. } = rx.try_recv().expect("submitted turn") {
            return items;
        }
    }
}

fn assert_no_turn(rx: &mut UnboundedReceiver<AppCommand>) {
    while let Ok(op) = rx.try_recv() {
        assert!(
            !matches!(op, AppCommand::UserTurn { .. }),
            "unexpected turn: {op:?}"
        );
    }
}

fn text(items: &[UserInput]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// These prompt tests use the current-thread runtime and do not yield between submission and
// completion/cancellation. The real owning submission path starts its async waiter, but taking
// its request aborts the waiter before it can start real pipe IO. The status test below also
// exercises an actually blocked worker and delivery through AppEvent.
#[tokio::test]
async fn prompt_completion_is_fresh_fifo_and_one_shot() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.ide_context.enable();
    chat.submit_user_message("first request".into());
    let first_id = pending_id(&chat);
    chat.submit_user_message("second request".into());
    assert_eq!(pending_id(&chat), first_id);
    assert_eq!(chat.input_queue.queued_user_messages.len(), 2);
    assert!(chat.bottom_pane.is_task_running());
    assert!(!chat.maybe_send_next_queued_input());
    assert_no_turn(&mut op_rx);
    chat.bottom_pane
        .set_composer_text("new draft".into(), vec![], vec![]);

    chat.on_ide_context_completed(first_id, Ok(context("first.rs")));
    let first = text(&next_turn(&mut op_rx));
    assert!(first.contains("first.rs"));
    assert!(first.ends_with("first request"));
    assert_eq!(chat.bottom_pane.composer_text(), "new draft");
    assert!(chat.input_queue.user_turn_pending_start);
    assert!(chat.bottom_pane.is_task_running());
    chat.on_ide_context_completed(first_id, Ok(context("stale.rs")));
    assert_no_turn(&mut op_rx);
    assert_eq!(chat.input_queue.queued_user_messages.len(), 1);

    let second_id = pending_id(&chat);
    assert_ne!(second_id, first_id);
    chat.on_ide_context_completed(second_id, Ok(context("second.rs")));
    let second = text(&next_turn(&mut op_rx));
    assert!(second.contains("second.rs"));
    assert!(!second.contains("first.rs"));
    assert!(second.ends_with("second request"));
    assert!(chat.input_queue.queued_user_messages.is_empty());
    assert_eq!(chat.bottom_pane.composer_text(), "new draft");
}

#[tokio::test]
async fn explicit_steers_remain_ahead_of_tab_queued_followups() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.ide_context.enable();
    chat.on_task_started();
    chat.submit_user_message("first steer".into());
    let first_id = pending_id(&chat);
    chat.queue_user_message("next turn".into());
    chat.submit_user_message("second steer".into());
    chat.on_ide_context_completed(first_id, Ok(context("first.rs")));
    let first = next_turn(&mut op_rx);
    assert!(chat.has_pending_steer(&first));
    let second_id = pending_id(&chat);
    chat.on_ide_context_completed(second_id, Ok(context("second.rs")));
    let second = next_turn(&mut op_rx);
    assert!(text(&second).ends_with("second steer"));
    assert!(chat.has_pending_steer(&second));
    assert_eq!(chat.input_queue.queued_user_messages.len(), 1);
    assert_eq!(
        chat.input_queue.queued_user_messages[0].user_message.text,
        "next turn"
    );
    assert!(!chat.ide_context.prompt_pending());
    assert!(!chat.maybe_send_next_queued_input());
}

#[tokio::test]
async fn deferred_shell_submission_does_not_fetch_or_overtake() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.ide_context.enable();
    chat.submit_user_message("first".into());
    let id = pending_id(&chat);
    chat.submit_user_message("!echo hello".into());
    assert!(op_rx.try_recv().is_err());
    chat.on_ide_context_completed(id, Ok(context("first.rs")));
    assert!(text(&next_turn(&mut op_rx)).ends_with("first"));
    let shell = op_rx.try_recv().expect("deferred shell command");
    assert!(
        matches!(shell, AppCommand::RunUserShellCommand { command } if command == "echo hello")
    );
    assert!(op_rx.try_recv().is_err());
    assert!(!chat.ide_context.prompt_pending());
    assert!(chat.input_queue.queued_user_messages.is_empty());
}

#[tokio::test]
async fn editing_deferred_tail_preserves_pending_head() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.ide_context.enable();
    chat.submit_user_message("first".into());
    let id = pending_id(&chat);
    chat.submit_user_message("second".into());
    assert_eq!(
        chat.pop_latest_queued_composer_state()
            .expect("second")
            .text,
        "second"
    );
    assert_eq!(pending_id(&chat), id);
    chat.on_ide_context_completed(id, Ok(context("first.rs")));
    assert!(text(&next_turn(&mut op_rx)).ends_with("first"));
    assert!(!chat.ide_context.prompt_pending());
    assert!(chat.input_queue.queued_user_messages.is_empty());
}

#[tokio::test]
async fn prompt_cancellation_preserves_history_attachments_mentions_and_new_draft() {
    for action in 0..3 {
        let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
        chat.thread_id = Some(ThreadId::new());
        chat.ide_context.enable();
        let mut message = UserMessage::from("original $tool");
        message.remote_image_urls = vec!["https://example.com/image.png".into()];
        message.mention_bindings = vec![MentionBinding {
            sigil: '$',
            mention: "tool".into(),
            path: "app://tool".into(),
        }];
        let history = UserMessageHistoryRecord::Override(UserMessageHistoryOverride {
            text: "/goal original $tool".into(),
            text_elements: Vec::new(),
        });
        assert!(chat.submit_user_message_with_history_record(message.clone(), history));
        let id = pending_id(&chat);
        let pending_pastes = vec![("[Pasted Content 4 chars]".into(), "body".into())];
        chat.restore_composer_state(ComposerDraftSnapshot {
            text: "new draft [Pasted Content 4 chars]".into(),
            pending_pastes: pending_pastes.clone(),
            ..Default::default()
        });
        match action {
            0 => chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            1 => chat.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            _ => chat.handle_ide_command_args("off"),
        }
        let restored = chat.bottom_pane.composer_draft_snapshot();
        assert_eq!(
            restored.text,
            "/goal original $tool\nnew draft [Pasted Content 4 chars]"
        );
        assert_eq!(restored.remote_image_urls, message.remote_image_urls);
        assert_eq!(restored.mention_bindings, message.mention_bindings);
        assert_eq!(restored.pending_pastes, pending_pastes);
        assert!(!chat.ide_context.prompt_pending());
        assert!(!chat.bottom_pane.is_task_running());
        assert!(chat.input_queue.queued_user_messages.is_empty());
        chat.on_ide_context_completed(id, Ok(context("stale.rs")));
        assert_eq!(chat.bottom_pane.composer_draft_snapshot(), restored);
        assert_no_turn(&mut op_rx);
    }
}

#[tokio::test]
async fn prompt_cancellation_and_failure_preserve_held_keystroke() {
    for cancel in [false, true] {
        let (mut chat, _rx, op_rx) = make_chatwidget_manual(None).await;
        chat.thread_id = Some(ThreadId::new());
        chat.ide_context.enable();
        chat.submit_user_message("original".into());
        let id = pending_id(&chat);
        chat.handle_key_event(KeyEvent::from(KeyCode::Char('a')));
        assert!(chat.bottom_pane.is_in_paste_burst());
        drop(op_rx);
        if cancel {
            chat.handle_ide_command_args("off");
        } else {
            chat.on_ide_context_completed(id, Ok(context("fresh.rs")));
        }
        assert_eq!(chat.bottom_pane.composer_text(), "original\na");
        assert!(!chat.ide_context.prompt_pending());
        assert!(!chat.bottom_pane.is_in_paste_burst());
    }
}

#[tokio::test]
async fn prompt_dispatch_failure_merges_newer_draft() {
    let (mut chat, _rx, op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.ide_context.enable();
    chat.submit_user_message("original".into());
    let id = pending_id(&chat);
    chat.bottom_pane
        .set_composer_text("new draft".into(), vec![], vec![]);
    drop(op_rx);
    chat.on_ide_context_completed(id, Ok(context("fresh.rs")));
    assert_eq!(chat.bottom_pane.composer_text(), "original\nnew draft");
    assert!(!chat.ide_context.prompt_pending());
    assert!(!chat.bottom_pane.is_task_running());
    assert!(!chat.input_queue.user_turn_pending_start);
    assert!(chat.ide_context.prompt_result.is_none());
}

#[tokio::test]
async fn prompt_snapshot_survives_switch_and_rejects_old_same_cwd_completion() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.ide_context.enable();
    chat.submit_user_message_as_plain_user_turn("!not a shell command".into());
    let id = pending_id(&chat);
    let snapshot = chat.capture_thread_input_state();
    chat.restore_thread_input_state(None);
    chat.thread_id = Some(ThreadId::new());
    chat.on_ide_context_completed(id, Ok(context("stale.rs")));
    assert_no_turn(&mut op_rx);
    assert!(chat.bottom_pane.composer_is_empty());
    assert!(!chat.bottom_pane.is_task_running());

    chat.thread_id = Some(thread_id);
    chat.restore_thread_input_state(snapshot);
    assert_eq!(chat.input_queue.queued_user_messages.len(), 1);
    assert_eq!(
        chat.input_queue.queued_user_messages[0].shell_escape_policy,
        ShellEscapePolicy::Disallow
    );
    chat.on_ide_context_completed(id, Ok(context("also-stale.rs")));
    assert_eq!(chat.input_queue.queued_user_messages.len(), 1);
    assert!(chat.maybe_send_next_queued_input());
    let new_id = pending_id(&chat);
    assert_ne!(id, new_id);
    chat.on_ide_context_completed(new_id, Ok(context("fresh.rs")));
    assert!(text(&next_turn(&mut op_rx)).ends_with("!not a shell command"));
}

#[tokio::test]
async fn prompt_cwd_change_and_queue_edit_invalidate_completion() {
    for edit_queue in [false, true] {
        let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
        chat.thread_id = Some(ThreadId::new());
        chat.ide_context.enable();
        chat.submit_user_message("original".into());
        let id = pending_id(&chat);
        if edit_queue {
            let draft = chat
                .pop_latest_queued_composer_state()
                .expect("queued draft");
            chat.restore_composer_state(draft);
        } else {
            chat.config.cwd = AbsolutePathBuf::try_from(chat.config.cwd.as_path().join("other"))
                .expect("absolute cwd");
        }
        chat.on_ide_context_completed(id, Ok(context("stale.rs")));
        assert_eq!(chat.bottom_pane.composer_text(), "original");
        assert!(!chat.ide_context.prompt_pending());
        assert!(!chat.bottom_pane.is_task_running());
        assert_no_turn(&mut op_rx);
    }
}

#[tokio::test]
async fn prompt_failure_submits_without_context_and_steer_key_uses_fresh_context() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.ide_context.enable();
    chat.on_task_started();
    chat.submit_user_message("steer one".into());
    let id = pending_id(&chat);
    chat.on_ide_context_completed(id, Err("test timeout".into()));
    let items = next_turn(&mut op_rx);
    assert_eq!(text(&items), "steer one");
    assert!(chat.has_pending_steer(&items));
    assert!(chat.ide_context.prompt_fetch_warned);
    assert!(!chat.input_queue.user_turn_pending_start);
    assert!(chat.bottom_pane.is_task_running());

    chat.submit_user_message("steer two".into());
    let id = pending_id(&chat);
    chat.on_ide_context_completed(id, Ok(context("fresh.rs")));
    let items = next_turn(&mut op_rx);
    assert!(text(&items).contains("fresh.rs"));
    assert!(chat.has_pending_steer(&items));
    assert!(!chat.ide_context.prompt_fetch_warned);
    assert!(!chat.input_queue.user_turn_pending_start);
}

#[tokio::test]
async fn initial_enable_failure_cancels_unsent_prompt() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.handle_ide_command_args_with_fetch("on", |_| Err("test failure".into()));
    let status_id = chat
        .ide_context
        .status_request
        .as_ref()
        .expect("status request")
        .id;
    chat.submit_user_message("original".into());
    let id = pending_id(&chat);
    chat.on_ide_context_completed(status_id, Err("test failure".into()));
    assert!(!chat.ide_context.is_enabled());
    assert!(!chat.bottom_pane.is_task_running());
    assert_eq!(chat.bottom_pane.composer_text(), "original");
    chat.on_ide_context_completed(id, Ok(context("stale.rs")));
    assert_no_turn(&mut op_rx);
}

#[tokio::test]
async fn status_worker_does_not_block_input_and_off_rejects_stale_result() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    chat.handle_ide_command_args_with_fetch("on", move |_| {
        let _ = started_tx.send(());
        release_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("release worker");
        Ok(context("worker.rs"))
    });
    let id = chat
        .ide_context
        .status_request
        .as_ref()
        .expect("status request")
        .id;
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("worker started without blocking runtime")
        .expect("started signal");
    chat.handle_ide_command_args_with_fetch("status", |_| panic!("duplicate worker"));
    assert_eq!(
        chat.ide_context
            .status_request
            .as_ref()
            .expect("coalesced status")
            .id,
        id
    );
    chat.handle_paste("x".into());
    assert_eq!(chat.bottom_pane.composer_text(), "x");
    release_tx.send(()).expect("release worker");
    let (completed_id, result) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let AppEvent::IdeContextCompleted { id, result } = rx.recv().await.expect("event") {
                break (id, result);
            }
        }
    })
    .await
    .expect("worker completion");
    assert_eq!(completed_id, id);
    chat.handle_ide_command_args("off");
    while rx.try_recv().is_ok() {}
    chat.on_ide_context_completed(completed_id, result);
    // An accepted status result always reports through a history cell.
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(event, AppEvent::InsertHistoryCell(_)),
            "stale status result must not be reported"
        );
    }
    assert!(!chat.ide_context.is_enabled());
    assert_eq!(chat.bottom_pane.composer_text(), "x");
    assert_no_turn(&mut op_rx);
}
