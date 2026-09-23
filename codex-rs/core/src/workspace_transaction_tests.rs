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
    let result = reconcile(&home, "task")?;
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
    let result = reconcile(&home, "task")?;
    assert!(!result.merged);
    assert_eq!(result.conflicts, ["source.txt"]);
    assert_eq!(fs::read_to_string(repo.join("source.txt"))?, "other\n");
    assert!(!repo.join("added.txt").exists());
    assert_eq!(fs::read_to_string(tx.workdir.join("source.txt"))?, "task\n");
    assert!(!load(&home, "task")?.unwrap().reconciled);
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
    assert_eq!(args["force_fresh"], true);
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
