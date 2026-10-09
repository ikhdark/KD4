use super::*;

#[tokio::test]
async fn goal_summary_status_snapshots() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(None).await;
    let thread_id = ThreadId::new();
    for (status, budget, snapshot) in [
        (AppThreadGoalStatus::Active, Some(80_000), "goal_menu_active"),
        (AppThreadGoalStatus::Paused, None, "goal_menu_paused"),
        (AppThreadGoalStatus::Blocked, None, "goal_menu_blocked"),
        (AppThreadGoalStatus::UsageLimited, None, "goal_menu_usage_limited"),
        (AppThreadGoalStatus::BudgetLimited, Some(80_000), "goal_menu_budget_limited"),
    ] {
        chat.show_goal_summary(test_goal(thread_id, status, budget));
        assert_chatwidget_snapshot!(snapshot, rendered_goal_summary(&mut rx));
    }
}

#[tokio::test]
async fn goal_edit_prompt_preserves_budget_and_resumable_status() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(None).await;
    let thread_id = ThreadId::new();
    for (initial, expected) in [
        (AppThreadGoalStatus::Active, AppThreadGoalStatus::Active),
        (AppThreadGoalStatus::Paused, AppThreadGoalStatus::Paused),
        (AppThreadGoalStatus::Blocked, AppThreadGoalStatus::Blocked),
        (AppThreadGoalStatus::UsageLimited, AppThreadGoalStatus::UsageLimited),
        (AppThreadGoalStatus::BudgetLimited, AppThreadGoalStatus::Active),
        (AppThreadGoalStatus::Complete, AppThreadGoalStatus::Active),
    ] {
        chat.show_goal_edit_prompt(thread_id, test_goal(thread_id, initial, Some(80_000)));
        if initial == AppThreadGoalStatus::Active {
            assert_chatwidget_snapshot!("goal_edit_prompt", render_bottom_popup(&chat, 100));
        }
        chat.handle_paste(" with clearer wording".to_string());
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
        match rx.try_recv() {
            Ok(AppEvent::SetThreadGoalDraft {
                thread_id: actual_thread,
                draft,
                mode: crate::app_event::ThreadGoalSetMode::UpdateExisting { status, token_budget },
            }) => {
                assert_eq!(actual_thread, thread_id);
                assert_eq!(draft.objective, "Keep improving the bare goal command until it feels calm and useful. with clearer wording");
                assert_eq!(status, expected);
                assert_eq!(token_budget, Some(80_000));
            }
            other => panic!("expected goal update, got {other:?}"),
        }
        assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));
        assert!(chat.no_modal_or_popup_active());
    }
}

#[tokio::test]
async fn resume_paused_goal_prompt_choices() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(None).await;
    let thread_id = ThreadId::new();
    for resume in [true, false] {
        chat.show_resume_paused_goal_prompt(
            thread_id,
            "Keep improving the bare goal command until it feels calm and useful.".to_string(),
        );
        if resume {
            assert_chatwidget_snapshot!("resume_paused_goal_prompt", render_bottom_popup(&chat, 100));
        } else {
            chat.handle_key_event(KeyEvent::from(KeyCode::Down));
        }
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
        if resume {
            assert_matches!(rx.try_recv(), Ok(AppEvent::SetThreadGoalStatus {
                thread_id: actual_thread,
                status: AppThreadGoalStatus::Active,
            }) if actual_thread == thread_id);
        }
        assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));
        assert!(chat.no_modal_or_popup_active());
    }
}

fn test_goal(
    thread_id: ThreadId,
    status: AppThreadGoalStatus,
    token_budget: Option<i64>,
) -> AppThreadGoal {
    AppThreadGoal {
        thread_id: thread_id.to_string(),
        objective: "Keep improving the bare goal command until it feels calm and useful."
            .to_string(),
        status,
        token_budget,
        tokens_used: 12_500,
        time_used_seconds: 90,
        created_at: 1_776_272_400,
        updated_at: 1_776_272_460,
    }
}

fn rendered_goal_summary(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::app_event::AppEvent>,
) -> String {
    drain_insert_history(rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n")
}
