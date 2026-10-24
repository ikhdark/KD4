use super::*;
use crate::session::tests::make_session_and_context_with_dynamic_tools_and_rx;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_exec_server::LOCAL_FS;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test]
async fn command_invalidation_is_published_without_turn_finalization() {
    let (session, turn, events) = make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
    let root = tempfile::tempdir().unwrap();
    let cwd = PathUri::from_host_native_path(root.path()).unwrap();
    let delta = codex_apply_patch::apply_patch("*** Begin Patch\n*** Add File: owned\n+owned\n*** End Patch",
        &cwd, &mut Vec::new(), &mut Vec::new(), LOCAL_FS.as_ref(), None).await.unwrap();
    let mut tracker = TurnDiffTracker::with_environment_display_roots([("local".into(), root.path().into())]);
    tracker.track_delta("local", &delta);
    assert!(tracker.take_unified_diff_if_changed().unwrap().contains("+owned"));
    let tracker = Arc::new(Mutex::new(tracker));
    std::fs::write(root.path().join("owned"), "command output\n").unwrap();
    let command = vec!["python".into(), "mutate.py".into()];
    emit_exec_end(ToolEventCtx::new(&session, &turn, "mutator", Some(&tracker)),
        ExecCommandInput::new(&command, &cwd, "local", &[], ExecCommandSource::Agent, None, None),
        ExecCommandResult { stdout: String::new(), stderr: String::new(), aggregated_output: String::new(),
            exit_code: Some(0), duration: Duration::ZERO, formatted_output: String::new(),
            status: ExecCommandStatus::Completed, timed_out: false }).await.unwrap();
    let messages = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(messages.iter().any(|event| matches!(&event.msg, EventMsg::TurnDiff(diff) if diff.unified_diff.is_empty())));
    assert!(messages.iter().any(|event| matches!(&event.msg, EventMsg::ItemCompleted(item)
        if matches!(&item.item, TurnItem::CommandExecution(command) if command.id == "mutator"))));
    assert!(tracker.lock().await.exact_export_diff(None).is_err());
    assert_eq!(std::fs::read_to_string(root.path().join("owned")).unwrap(), "command output\n");
}
