use super::*;

fn fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
    let root = tempfile::tempdir()?;
    let repo = root.path().join("repo");
    let home = root.path().join("home");
    fs::create_dir_all(&repo)?;
    fs::create_dir_all(&home)?;
    fs::write(
        repo.join("source.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
    )?;
    fs::write(repo.join(".gitignore"), "target/\n")?;
    init_snapshot(&repo)?;
    Ok((root, repo, home))
}

#[test]
fn captures_dirty_and_untracked_source_without_changing_original_index() -> Result<()> {
    let (_root, repo, home) = fixture()?;
    fs::write(repo.join("source.txt"), "uncommitted\n")?;
    fs::write(repo.join("new.txt"), "new\n")?;
    fs::create_dir(repo.join("target"))?;
    fs::write(repo.join("target/artifact"), "ignored")?;
    let index = fs::read(repo.join(".git/index"))?;
    let head = git(&repo, &["rev-parse", "HEAD"])?.stdout;
    let tx = begin(&home, "task", &repo)?;
    assert_eq!(
        fs::read_to_string(tx.workdir.join("source.txt"))?,
        "uncommitted\n"
    );
    assert_eq!(fs::read_to_string(tx.workdir.join("new.txt"))?, "new\n");
    assert!(!tx.workdir.join("target").exists());
    fs::write(tx.workdir.join("source.txt"), "isolated\n")?;
    assert_eq!(
        fs::read_to_string(repo.join("source.txt"))?,
        "uncommitted\n"
    );
    assert_eq!(begin(&home, "task", &repo)?.workdir, tx.workdir);
    assert_eq!(fs::read(repo.join(".git/index"))?, index);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"])?.stdout, head);
    Ok(())
}

#[test]
fn reconciliation_merges_independent_edits_and_preserves_index() -> Result<()> {
    let (_root, repo, home) = fixture()?;
    let tx = begin(&home, "task", &repo)?;
    let index = fs::read(repo.join(".git/index"))?;
    fs::write(
        tx.workdir.join("source.txt"),
        "TASK\ntwo\nthree\nfour\nfive\nsix\nseven\n",
    )?;
    fs::write(
        repo.join("source.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nOTHER\n",
    )?;
    fs::write(tx.workdir.join("added.txt"), "new file\n")?;
    let result = reconcile(&home, "task", &BTreeMap::new())?;
    assert!(result.merged);
    assert!(result.validation_required);
    assert_eq!(
        fs::read_to_string(repo.join("source.txt"))?,
        "TASK\ntwo\nthree\nfour\nfive\nsix\nOTHER\n"
    );
    assert_eq!(fs::read_to_string(repo.join("added.txt"))?, "new file\n");
    assert_eq!(fs::read(repo.join(".git/index"))?, index);
    assert!(load(&home, "task")?.unwrap().reconciled);
    Ok(())
}

#[test]
fn conflicts_publish_no_files_and_retain_task_edits() -> Result<()> {
    let (_root, repo, home) = fixture()?;
    let tx = begin(&home, "task", &repo)?;
    fs::write(tx.workdir.join("source.txt"), "task\n")?;
    fs::write(tx.workdir.join("added.txt"), "must not publish\n")?;
    fs::write(repo.join("source.txt"), "other\n")?;
    let result = reconcile(&home, "task", &BTreeMap::new())?;
    assert!(!result.merged);
    assert_eq!(result.conflicts, ["source.txt"]);
    let inputs = result
        .conflict_directory
        .as_ref()
        .expect("retained conflict inputs");
    assert_eq!(
        fs::read_to_string(inputs.join("current/source.txt"))?,
        "other\n"
    );
    assert_eq!(
        fs::read_to_string(inputs.join("task/source.txt"))?,
        "task\n"
    );
    assert_eq!(
        fs::read(inputs.join("base/source.txt"))?,
        fs::read(tx.base.join("source.txt"))?
    );
    fs::write(repo.join("source.txt"), "newer concurrent edit\n")?;
    assert_eq!(
        fs::read_to_string(inputs.join("current/source.txt"))?,
        "other\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("source.txt"))?,
        "newer concurrent edit\n"
    );
    assert!(!repo.join("added.txt").exists());
    assert_eq!(fs::read_to_string(tx.workdir.join("source.txt"))?, "task\n");
    assert!(!load(&home, "task")?.unwrap().reconciled);
    Ok(())
}

#[test]
fn reconciliation_merges_through_a_task_line_ending_conversion() -> Result<()> {
    let crlf = "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\n";
    let current = crlf.replacen("one", "CURRENT", 1);
    for (thread, task, merged, published) in [
        // A formatter rewrote the task file as LF while editing its last line.
        (
            "task-edit",
            "one\ntwo\nthree\nfour\nfive\nsix\nTASK\n",
            Some("CURRENT\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nTASK\r\n"),
            true,
        ),
        // Only the line endings changed, so the current file is already the result.
        (
            "conversion-only",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
            Some(current.as_str()),
            false,
        ),
        // Content changes to the same line still conflict.
        (
            "overlap",
            "TASK\ntwo\nthree\nfour\nfive\nsix\nseven\n",
            None,
            false,
        ),
    ] {
        let (_root, repo, home) = fixture()?;
        fs::write(repo.join("crlf.txt"), crlf)?;
        let tx = begin(&home, thread, &repo)?;
        fs::write(tx.workdir.join("crlf.txt"), task)?;
        fs::write(repo.join("crlf.txt"), &current)?;
        let result = reconcile(&home, thread, &BTreeMap::new())?;
        match merged {
            Some(expected) => {
                assert!(result.merged, "{thread}: {result:?}");
                assert_eq!(
                    fs::read_to_string(repo.join("crlf.txt"))?,
                    expected,
                    "{thread}"
                );
                assert_eq!(
                    result.changed_paths.contains(&"crlf.txt".to_string()),
                    published
                );
            }
            None => {
                assert!(!result.merged, "{thread}: {result:?}");
                assert_eq!(result.conflicts, ["crlf.txt"]);
                assert_eq!(fs::read_to_string(repo.join("crlf.txt"))?, current);
            }
        }
    }
    Ok(())
}

#[test]
fn line_endings_are_normalized_only_for_uniform_text() {
    let normalized = |base: &[u8], current: &[u8], task: &[u8]| {
        merge_line_endings(base, current, task).into_owned()
    };
    assert_eq!(
        normalized(b"a\r\nb\r\n", b"a\r\nc\r\n", b"a\nb\n"),
        b"a\r\nb\r\n"
    );
    assert_eq!(normalized(b"a\nb\n", b"c\nb\n", b"a\r\nd\r\n"), b"a\nd\n");
    // Mixed task endings, a current file in another style, and binary data
    // are merged exactly as written.
    assert_eq!(
        normalized(b"a\r\nb\r\n", b"a\r\nc\r\n", b"a\nb\r\n"),
        b"a\nb\r\n"
    );
    assert_eq!(normalized(b"a\r\nb\r\n", b"a\nc\n", b"a\nb\n"), b"a\nb\n");
    assert_eq!(normalized(b"\0a\r\n", b"\0b\r\n", b"\0a\n"), b"\0a\n");
}

#[test]
fn snapshot_git_stderr_omits_line_ending_warnings() -> Result<()> {
    let (_root, repo, _home) = fixture()?;
    fs::write(repo.join(".gitattributes"), "* text=auto eol=lf\n")?;
    fs::write(repo.join("control.txt"), "one\r\ntwo\r\n")?;
    fs::write(repo.join("snapshot.txt"), "one\r\ntwo\r\n")?;
    // The fixture produces the warning that buried snapshot failures.
    let control = Command::new("git")
        .current_dir(&repo)
        .args(["-c", "core.safecrlf=warn", "-c", "core.autocrlf=false"])
        .args(["add", "control.txt"])
        .output()?;
    assert!(control.status.success());
    assert!(String::from_utf8_lossy(&control.stderr).contains("CRLF will be replaced by LF"));
    let output = git(&repo, &["add", "snapshot.txt"])?;
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    Ok(())
}

#[cfg(windows)]
#[test]
fn snapshots_capture_long_paths_without_changing_origin_git_configuration() -> Result<()> {
    let (_root, repo, home) = fixture()?;
    let relative = format!("{}/{}/source.txt", "a".repeat(110), "b".repeat(110));
    let path = repo.join(&relative);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, "long path source\n")?;
    let config = fs::read(repo.join(".git/config"))?;
    let index = fs::read(repo.join(".git/index"))?;
    let tx = begin(&home, "long-path", &repo)?;
    assert!(tx.workdir.join(&relative).as_os_str().len() > 260);
    assert_eq!(
        fs::read_to_string(tx.workdir.join(&relative))?,
        "long path source\n"
    );
    let snapshot = validation_snapshot(&home, "long-path")?;
    assert_eq!(
        fs::read_to_string(snapshot.workdir.join(&relative))?,
        "long path source\n"
    );
    assert_eq!(fs::read(repo.join(".git/config"))?, config);
    assert_eq!(fs::read(repo.join(".git/index"))?, index);
    Ok(())
}

#[test]
fn validation_snapshots_bind_inputs_and_separate_artifacts() -> Result<()> {
    let (_root, repo, home) = fixture()?;
    let tx = begin(&home, "task", &repo)?;
    let first = validation_snapshot(&home, "task")?;
    fs::write(tx.workdir.join("source.txt"), "next revision\n")?;
    let second = validation_snapshot(&home, "task")?;
    assert_ne!(first.revision, second.revision);
    assert_ne!(first.workdir, second.workdir);
    assert_ne!(
        fs::read(first.workdir.join("source.txt"))?,
        fs::read(second.workdir.join("source.txt"))?
    );
    let mut env = std::collections::HashMap::new();
    bind_validation_environment(&first, &mut env);
    let first_target = env["CARGO_TARGET_DIR"].clone();
    bind_validation_environment(&second, &mut env);
    assert_ne!(env["CARGO_TARGET_DIR"], first_target);
    assert_eq!(env["CARGO_TARGET_DIR"], env["CODEX_CARGO_LANE_TARGET_DIR"]);
    assert_eq!(env["CODEX_VALIDATION_SOURCE_REVISION"], second.revision);
    verify_validation_snapshot(&first)?;
    fs::write(
        first.workdir.join("source.txt"),
        "changed during validation\n",
    )?;
    assert!(verify_validation_snapshot(&first).is_err());
    Ok(())
}

#[test]
fn validation_lanes_stay_warm_across_snapshots_and_refresh_captured_inputs() -> Result<()> {
    let (_root, repo, home) = fixture()?;
    begin(&home, "task", &repo)?;
    let target = |lane: &ValidationLane, snapshot: &WorkspaceTransaction| {
        let mut env = std::collections::HashMap::new();
        bind_validation_environment(snapshot, &mut env);
        lane.bind(&mut env);
        assert_eq!(env["CARGO_TARGET_DIR"], env["CODEX_CARGO_LANE_TARGET_DIR"]);
        assert_eq!(env["CODEX_VALIDATION_SOURCE_REVISION"], snapshot.revision);
        PathBuf::from(&env["CARGO_TARGET_DIR"])
    };

    let first = validation_snapshot(&home, "task")?;
    let captured = first.workdir.join("source.txt");
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&captured)?
        .set_modified(stale)?;
    let first_lane = lease_validation_lane(&home, "task", &first)?.expect("idle lane");
    let warm = target(&first_lane, &first);
    assert!(warm.starts_with(home.join("validation-cache")));
    assert!(!warm.starts_with(first.workdir.parent().unwrap()));
    // Inputs captured before the lease are newer than any build in the lane.
    assert!(fs::metadata(&captured)?.modified()? > stale);

    // A concurrent build of the same checkout takes another lane.
    let second = validation_snapshot(&home, "task")?;
    let second_lane = lease_validation_lane(&home, "task", &second)?.expect("overflow lane");
    assert_ne!(target(&second_lane, &second), warm);
    drop((first_lane, second_lane));

    // A later snapshot, even from another task, returns to the warm lane.
    let (_other_root, other_repo, _) = fixture()?;
    let tx = begin(&home, "later", &repo)?;
    fs::write(tx.workdir.join("source.txt"), "later edit\n")?;
    let later = validation_snapshot(&home, "later")?;
    let later_lane = lease_validation_lane(&home, "later", &later)?.expect("idle lane");
    assert_eq!(target(&later_lane, &later), warm);
    drop(later_lane);

    // Another checkout never shares build outputs.
    begin(&home, "other", &other_repo)?;
    let other = validation_snapshot(&home, "other")?;
    let other_lane = lease_validation_lane(&home, "other", &other)?.expect("idle lane");
    assert!(!target(&other_lane, &other).starts_with(warm.parent().unwrap()));

    // A saturated checkout keeps the snapshot's own output directory.
    let leases = (0..4)
        .map(|_| lease_validation_lane(&home, "task", &first).map(Option::unwrap))
        .collect::<Result<Vec<_>>>()?;
    assert!(lease_validation_lane(&home, "task", &first)?.is_none());
    drop(leases);
    Ok(())
}

#[test]
fn native_routing_maps_reads_patches_and_validation() -> Result<()> {
    use crate::tools::context::ToolPayload;
    let (_root, repo, home) = fixture()?;
    let tx = begin(&home, "task", &repo)?;
    let mut read = ToolPayload::Function {
        arguments: serde_json::json!({"path":repo.join("source.txt")}).to_string(),
    };
    route_call(&home, "task", &repo, "read_file", &mut read)?;
    let ToolPayload::Function { arguments } = read else {
        panic!("function")
    };
    let args: serde_json::Value = serde_json::from_str(&arguments)?;
    assert_eq!(
        PathBuf::from(args["path"].as_str().unwrap()),
        tx.workdir.join("source.txt")
    );
    let mut patch = ToolPayload::Custom {
        input: "*** Begin Patch\n*** Delete File: source.txt\n*** End Patch\n".into(),
    };
    route_call(&home, "task", &repo, "apply_patch", &mut patch)?;
    let ToolPayload::Custom { input } = patch else {
        panic!("custom")
    };
    assert!(input.contains(&tx.workdir.join("source.txt").to_string_lossy().to_string()));
    let mut command = ToolPayload::Function {
        arguments: serde_json::json!({"cmd":"cargo test --lib", "workdir":repo}).to_string(),
    };
    route_call(&home, "task", &repo, "exec_command", &mut command)?;
    let ToolPayload::Function { arguments } = command else {
        panic!("function")
    };
    let args: serde_json::Value = serde_json::from_str(&arguments)?;
    let cwd = PathBuf::from(args["workdir"].as_str().unwrap());
    assert_ne!(dunce::simplified(&cwd), dunce::simplified(&tx.workdir));
    assert!(args.get("force_fresh").is_none());
    use crate::tools::registry::ToolExecutor;
    let codex_tools::ToolSpec::Function(spec) =
        crate::tools::handlers::ExecCommandHandler::default().spec()
    else {
        panic!("exec command schema")
    };
    jsonschema::validator_for(&serde_json::to_value(spec.parameters)?)?
        .validate(&args)
        .expect("routed validation must satisfy the public exec schema");
    assert!(validation_context(&home, "task", &cwd)?.is_some());
    let mut escape = ToolPayload::Custom {
        input: "*** Begin Patch\n*** Delete File: ../escape\n*** End Patch\n".into(),
    };
    assert!(route_call(&home, "task", &repo, "apply_patch", &mut escape).is_err());
    Ok(())
}

#[tokio::test]
async fn evidence_tracks_task_edits_instead_of_unrelated_origin_edits() -> Result<()> {
    let (_root, repo, home) = fixture()?;
    let (_session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    std::sync::Arc::make_mut(&mut turn.config).codex_home =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(home.clone())?;
    let thread = turn.session_telemetry.conversation_id().to_string();
    let tx = begin(&home, &thread, &repo)?;
    let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let first = cache
        .workspace_evidence_for_turn(&turn, &repo)
        .await
        .identity;
    assert!(first.is_some());
    fs::write(repo.join("source.txt"), "unrelated live change\n")?;
    let unchanged = cache
        .workspace_evidence_for_turn(&turn, &repo)
        .await
        .identity;
    assert_eq!(first, unchanged);
    fs::write(tx.workdir.join("source.txt"), "task edit\n")?;
    let changed = cache
        .workspace_evidence_for_turn(&turn, &repo)
        .await
        .identity;
    assert_ne!(first, changed);
    Ok(())
}

#[test]
fn manual_conflict_resolution_checks_revisions_and_preserves_unresolved_files() -> Result<()> {
    use crate::tools::context::ToolPayload;
    let (_root, repo, home) = fixture()?;
    let tx = begin(&home, "task", &repo)?;
    let index = fs::read(repo.join(".git/index"))?;
    fs::write(tx.workdir.join("source.txt"), "task\n")?;
    fs::write(tx.workdir.join("added.txt"), "task addition\n")?;
    fs::write(repo.join("source.txt"), "current\n")?;
    fs::write(repo.join("added.txt"), "concurrent addition\n")?;
    let conflict = reconcile(&home, "task", &BTreeMap::new())?;
    assert_eq!(conflict.conflicts, ["added.txt", "source.txt"]);
    assert_eq!(
        conflict.conflict_revisions["source.txt"],
        digest(b"current\n")
    );
    let retained = conflict
        .conflict_directory
        .unwrap()
        .join("current/source.txt");
    let mut read = ToolPayload::Function {
        arguments: serde_json::json!({"path": retained}).to_string(),
    };
    route_call(&home, "task", &repo, "read_file", &mut read)?;
    let ToolPayload::Function { arguments } = read else {
        panic!("function")
    };
    let read: serde_json::Value = serde_json::from_str(&arguments)?;
    assert_eq!(PathBuf::from(read["path"].as_str().unwrap()), retained);
    assert_eq!(fs::read_to_string(&retained)?, "current\n");

    fs::write(tx.workdir.join("source.txt"), "task and current\n")?;
    let partial = BTreeMap::from([(
        "source.txt".into(),
        conflict.conflict_revisions["source.txt"].clone(),
    )]);
    let unresolved = reconcile(&home, "task", &partial)?;
    assert!(!unresolved.merged);
    assert_eq!(unresolved.conflicts, ["added.txt"]);
    assert_eq!(fs::read_to_string(repo.join("source.txt"))?, "current\n");
    fs::write(tx.workdir.join("added.txt"), "both additions\n")?;
    fs::write(repo.join("source.txt"), "new current\n")?;
    let stale = reconcile(&home, "task", &conflict.conflict_revisions).unwrap_err();
    assert!(stale.to_string().contains("resolution is stale"));
    assert_eq!(
        fs::read_to_string(repo.join("added.txt"))?,
        "concurrent addition\n"
    );
    assert!(!load(&home, "task")?.unwrap().reconciled);

    let latest = reconcile(&home, "task", &BTreeMap::new())?;
    fs::write(tx.workdir.join("source.txt"), "task and new current\n")?;
    let merged = reconcile(&home, "task", &latest.conflict_revisions)?;
    assert!(merged.merged);
    assert!(merged.validation_required);
    assert_eq!(merged.changed_paths, ["added.txt", "source.txt"]);
    assert_eq!(
        fs::read_to_string(repo.join("source.txt"))?,
        "task and new current\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("added.txt"))?,
        "both additions\n"
    );
    assert_eq!(fs::read(repo.join(".git/index"))?, index);
    Ok(())
}

#[test]
fn manual_resolution_handles_deletion_and_rejects_unknown_paths() -> Result<()> {
    for delete_task in [false, true] {
        let (_root, repo, home) = fixture()?;
        let tx = begin(&home, "task", &repo)?;
        if delete_task {
            fs::remove_file(tx.workdir.join("source.txt"))?;
            fs::write(repo.join("source.txt"), "current\n")?;
        } else {
            fs::write(tx.workdir.join("source.txt"), "task\n")?;
            fs::remove_file(repo.join("source.txt"))?;
        }
        let conflict = reconcile(&home, "task", &BTreeMap::new())?;
        assert!(!conflict.merged);
        assert_eq!(
            conflict.conflict_revisions["source.txt"],
            if delete_task {
                digest(b"current\n")
            } else {
                "absent".into()
            }
        );
        let invalid = BTreeMap::from([("../outside".into(), "absent".into())]);
        assert!(
            reconcile(&home, "task", &invalid)
                .unwrap_err()
                .to_string()
                .contains("not a task source path")
        );
        assert_eq!(repo.join("source.txt").exists(), delete_task);
        assert!(reconcile(&home, "task", &conflict.conflict_revisions)?.merged);
        assert_eq!(repo.join("source.txt").exists(), !delete_task);
        if !delete_task {
            assert_eq!(fs::read_to_string(repo.join("source.txt"))?, "task\n");
        }
    }
    Ok(())
}

#[tokio::test]
async fn reconciliation_handler_accepts_schema_valid_manual_resolution() -> Result<()> {
    use crate::session::step_context::StepContext;
    use crate::tools::context::{ToolCallSource, ToolInvocation, ToolPayload};
    use crate::tools::registry::ToolExecutor;
    use std::sync::Arc;
    let (_root, repo, home) = fixture()?;
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    Arc::make_mut(&mut turn.config).codex_home =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(home.clone())?;
    let thread = session.thread_id.to_string();
    let tx = begin(&home, &thread, &repo)?;
    fs::write(tx.workdir.join("source.txt"), "task\n")?;
    fs::write(repo.join("source.txt"), "current\n")?;
    let conflict = reconcile(&home, &thread, &BTreeMap::new())?;
    fs::write(tx.workdir.join("source.txt"), "combined\n")?;
    let handler = crate::tools::handlers::WorkspaceTransactionHandler;
    let value =
        serde_json::json!({"action":"reconcile", "resolutions":conflict.conflict_revisions});
    let codex_tools::ToolSpec::Function(spec) = handler.spec() else {
        panic!("function schema")
    };
    jsonschema::validator_for(&serde_json::to_value(spec.parameters)?)?
        .validate(&value)
        .expect("resolution schema");
    let payload = ToolPayload::Function {
        arguments: value.to_string(),
    };
    let invocation = ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::new(turn)),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "manual-resolution".into(),
        tool_name: handler.tool_name(),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    };
    let output = handler.handle(invocation).await?;
    let result = output.code_mode_result(&payload);
    assert_eq!(result["merged"], true);
    assert_eq!(result["validation_required"], true);
    assert_eq!(fs::read_to_string(repo.join("source.txt"))?, "combined\n");
    Ok(())
}
