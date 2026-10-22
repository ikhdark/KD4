use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::CommandMutation;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::LOCAL_FS;
use codex_protocol::models::PermissionProfile;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

async fn apply_patch_in(root: &Path, patch: &str) -> codex_apply_patch::AppliedPatchDelta {
    let cwd = PathUri::from_host_native_path(root).expect("absolute tempdir path");
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    codex_apply_patch::apply_patch(
        patch,
        &cwd,
        &mut stdout,
        &mut stderr,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    .expect("patch should apply")
}

fn tracker_for(root: &Path) -> TurnDiffTracker {
    TurnDiffTracker::with_environment_display_roots([(
        LOCAL_ENVIRONMENT_ID.to_string(),
        root.to_path_buf(),
    )])
}

async fn invocation(root: &Path, tracker: TurnDiffTracker, name: &str) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    turn.environments.turn_environments = vec![TurnEnvironment::new(
        LOCAL_ENVIRONMENT_ID.into(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        PathUri::from_host_native_path(root).unwrap(),
        None,
    )];
    ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::new(turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(tracker)),
        call_id: "save-turn-diff".into(),
        tool_name: ToolName::plain(SAVE_TURN_DIFF_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({ "name": name }).to_string(),
        },
    }
}

#[tokio::test]
async fn saves_the_exact_tracked_diff_without_unrelated_writes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("edited.txt"), "before\n").unwrap();
    let mut tracker = tracker_for(root.path());
    for patch in [
        "*** Begin Patch\n*** Update File: edited.txt\n@@\n-before\n+after\n*** End Patch",
        "*** Begin Patch\n*** Add File: added.txt\n+new\n*** End Patch",
    ] {
        let delta = apply_patch_in(root.path(), patch).await;
        tracker.track_delta(LOCAL_ENVIRONMENT_ID, &delta);
    }
    // Another writer's file is neither this turn's change nor part of its patch.
    std::fs::write(root.path().join("unrelated.txt"), "other session\n").unwrap();
    let expected = tracker
        .exact_unified_diff()
        .expect("both edits are tracked exactly")
        .expect("the turn has edits");

    let call = invocation(root.path(), tracker, "two-edits").await;
    let payload = call.payload.clone();
    let output = SaveTurnDiffHandler.handle(call).await.expect("diff saved");

    let saved = root
        .path()
        .join(".codex")
        .join("gitdiff")
        .join("two-edits.patch");
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), expected);
    for line in ["a/edited.txt", "-before", "+after", "b/added.txt", "+new"] {
        assert!(expected.contains(line), "{line} missing from {expected}");
    }
    assert!(!expected.contains("unrelated.txt"));
    let result = output.code_mode_result(&payload);
    let reported = result["path"].as_str().unwrap().replace('\\', "/");
    assert!(
        reported.ends_with(".codex/gitdiff/two-edits.patch"),
        "{reported}"
    );
    assert_eq!(result["files"], 2);
    assert_eq!(result["bytes"], expected.len());
}

#[tokio::test]
async fn saved_diff_replays_content_preserving_moves_with_and_without_other_edits() {
    for include_addition in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("old name.txt"), "same\n").unwrap();
        let mut tracker = tracker_for(root.path());
        let delta = apply_patch_in(
            root.path(),
            "*** Begin Patch\n*** Update File: old name.txt\n*** Move to: new name.txt\n@@\n same\n*** End Patch",
        )
        .await;
        tracker.track_delta(LOCAL_ENVIRONMENT_ID, &delta);
        if include_addition {
            let delta = apply_patch_in(
                root.path(),
                "*** Begin Patch\n*** Add File: added.txt\n+new\n*** End Patch",
            )
            .await;
            tracker.track_delta(LOCAL_ENVIRONMENT_ID, &delta);
        }

        let call = invocation(root.path(), tracker, "rename").await;
        let payload = call.payload.clone();
        let output = SaveTurnDiffHandler.handle(call).await.expect("diff saved");
        assert_eq!(
            output.code_mode_result(&payload)["files"],
            if include_addition { 2 } else { 1 }
        );

        // The oracle is the requested filesystem transition, not another
        // rendering of the tracker's output. Replay with the real consumer.
        let replay = tempfile::tempdir().unwrap();
        std::fs::write(replay.path().join("old name.txt"), "same\n").unwrap();
        let applied = std::process::Command::new("git")
            .args(["-c", "core.autocrlf=false", "apply", "--whitespace=nowarn"])
            .arg(root.path().join(".codex/gitdiff/rename.patch"))
            .current_dir(replay.path())
            .output()
            .expect("run git apply");
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert!(!replay.path().join("old name.txt").exists());
        assert_eq!(
            std::fs::read(replay.path().join("new name.txt")).unwrap(),
            b"same\n"
        );
        assert_eq!(replay.path().join("added.txt").exists(), include_addition);
        if include_addition {
            assert_eq!(
                std::fs::read(replay.path().join("added.txt")).unwrap(),
                b"new\n"
            );
        }
    }
}

#[tokio::test]
async fn refuses_a_partial_diff_and_names_the_untracked_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut tracker = tracker_for(root.path());
    let delta = apply_patch_in(
        root.path(),
        "*** Begin Patch\n*** Add File: added.txt\n+new\n*** End Patch",
    )
    .await;
    tracker.track_delta(LOCAL_ENVIRONMENT_ID, &delta);
    // A script wrote a file the tracker has no exact diff for.
    tracker.record_exec_command_end_with_mutation_at(
        &["python".to_string(), "generate.py".to_string()],
        /*exit_code*/ 0,
        /*timed_out*/ false,
        LOCAL_ENVIRONMENT_ID,
        Some(root.path()),
        CommandMutation::KnownMutation {
            paths: Some(BTreeSet::from([root.path().join("generated.txt")])),
        },
    );

    let result = SaveTurnDiffHandler
        .handle(invocation(root.path(), tracker, "partial").await)
        .await;
    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("a partial diff must not be saved");
    };
    assert!(message.contains("generated.txt"), "{message}");
    assert!(!root.path().join(".codex").exists(), "nothing was written");
}

#[tokio::test]
async fn audit107_refuses_foreign_hunks_between_exact_patches_without_exec() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("edited.txt");
    std::fs::write(&path, "own = 1\nother = 7\n").unwrap();
    let save_call = invocation(root.path(), tracker_for(root.path()), "foreign").await;
    let handler = crate::tools::handlers::apply_patch::ApplyPatchHandler::new(true);
    for (ordinal, old, new) in [(1, 1, 2), (2, 2, 3)] {
        if ordinal == 2 {
            // Deliberately no exec/command reconciliation between patch calls.
            std::fs::write(&path, "own = 2\nother = 9\n").unwrap();
        }
        let mut patch_call = save_call.clone();
        patch_call.call_id = format!("audit107-patch-{ordinal}");
        patch_call.tool_name = ToolName::plain("apply_patch");
        patch_call.payload = ToolPayload::Custom {
            input: format!(
                "*** Begin Patch\n*** Update File: edited.txt\n@@\n-own = {old}\n+own = {new}\n*** End Patch"
            ),
        };
        let payload = patch_call.payload.clone();
        let output = handler.handle(patch_call).await.unwrap();
        assert_eq!(output.code_mode_result(&payload)["success"], true);
    }
    // Exercise the production patch handler -> emitter -> tracker -> exporter.
    let result = SaveTurnDiffHandler.handle(save_call).await;
    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("foreign ownership must not be exported as exact");
    };
    assert!(
        message.contains("partial") && message.contains("edited.txt"),
        "{message}"
    );
    assert!(!root.path().join(".codex").exists());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "own = 3\nother = 9\n"
    );
}

#[tokio::test]
async fn refuses_a_turn_without_tracked_edits() {
    let root = tempfile::tempdir().unwrap();
    let result = SaveTurnDiffHandler
        .handle(invocation(root.path(), tracker_for(root.path()), "empty").await)
        .await;
    let Err(FunctionCallError::RespondToModel(message)) = result else {
        panic!("an empty diff must not be saved");
    };
    assert!(
        message.contains("no tracked apply_patch edits"),
        "{message}"
    );
    assert!(!root.path().join(".codex").exists());
}

#[test]
fn patch_names_stay_inside_the_gitdiff_directory() {
    for valid in [
        "fix-login",
        "H6_save.turn-diff",
        &"a".repeat(MAX_NAME_CHARS),
    ] {
        assert!(validated_patch_name(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        ".hidden",
        "../escape",
        "nested/name",
        "back\\slash",
        "space name",
        &"a".repeat(MAX_NAME_CHARS + 1),
    ] {
        assert!(validated_patch_name(invalid).is_err(), "{invalid}");
    }
}
