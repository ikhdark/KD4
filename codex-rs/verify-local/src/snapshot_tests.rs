use super::*;

#[test]
fn porcelain_v2_preserves_rename_paths_and_sorts_raw_records() {
    let bytes = b"? z new\npath\0\
1 M. N... 100644 100644 100644 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb a.rs\0\
2 R. N... 100644 100644 100644 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb R100 new.rs\0old.rs\0";
    let mut records = parse_porcelain_v2(bytes).expect("parse status");
    sort_records(&mut records);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].status, "?");
    assert_eq!(records[1].status, "M.");
    assert_eq!(records[2].status, "R.");
    assert_eq!(records[2].path.as_bytes(), b"new.rs");
    assert_eq!(
        records[2].original_path.as_ref().map(RawPath::as_bytes),
        Some(b"old.rs".as_slice())
    );
    assert_eq!(records[0].path.as_bytes(), b"z new\npath");
}

#[test]
fn commit_diff_preserves_rename_old_and_new_independently() {
    let records =
        parse_name_status(b"M\0plain.rs\0R087\0old name.rs\0new name.rs\0").expect("parse diff");
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].status, "R087");
    assert_eq!(records[1].path.as_bytes(), b"new name.rs");
    assert_eq!(
        records[1].original_path.as_ref().map(RawPath::as_bytes),
        Some(b"old name.rs".as_slice())
    );
}

#[test]
fn copy_status_is_unsupported_input() {
    assert!(matches!(
        parse_name_status(b"C100\0old\0new\0"),
        Err(SnapshotError::UnsupportedCopy)
    ));
}

#[test]
fn malformed_nul_output_fails_closed() {
    assert!(matches!(
        parse_name_status(b"M\0missing-final-nul"),
        Err(SnapshotError::Malformed(_))
    ));
}

#[test]
fn conflicts_and_ambiguous_merge_bases_are_fail_closed() {
    let oid = "a".repeat(40);
    let conflict = format!(
        "u UU N... 100644 100644 100644 100644 {oid} {oid} {oid} conflict.rs\0"
    );
    let records = parse_porcelain_v2(conflict.as_bytes()).expect("conflict");
    assert_eq!(records[0].status, "UU");
    assert!(matches!(
        parse_single_merge_base(format!("{oid}\n{}\n", "b".repeat(40)).as_bytes()),
        Err(SnapshotError::AmbiguousMergeBase { count: 2 })
    ));
}

#[test]
fn unknown_statuses_and_invalid_rename_scores_fail_closed() {
    for bytes in [
        b"X\0path\0".as_slice(),
        b"U\0path\0".as_slice(),
        b"R101\0old\0new\0".as_slice(),
        b"R1\0old\0new\0".as_slice(),
    ] {
        assert!(matches!(
            parse_name_status(bytes),
            Err(SnapshotError::Malformed(_))
        ));
    }
    assert!(matches!(
        parse_porcelain_v2(b"! ignored\0"),
        Err(SnapshotError::Malformed(_))
    ));
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_are_lossless_on_unix() {
    let records = parse_name_status(b"A\0raw-\xff-name\0").expect("parse raw path");
    assert_eq!(records[0].path.as_bytes(), b"raw-\xff-name");
    assert!(records[0].path.as_utf8().is_none());
}

#[test]
fn explicit_paths_are_deterministic_and_do_not_require_git() {
    let temp = tempfile::tempdir().expect("tempdir");
    let snapshot = RepositorySnapshot::from_explicit_paths(
        temp.path(),
        [RawPath::from_utf8("z.rs"), RawPath::from_utf8("a.rs")],
    )
    .expect("explicit snapshot");
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|record| record.path.as_utf8())
            .collect::<Vec<_>>(),
        vec![Some("a.rs"), Some("z.rs")]
    );
    assert_eq!(snapshot.source, SnapshotSource::ExplicitPaths);
}

#[test]
fn worktree_and_direct_commit_diff_cover_rename_and_deletion() {
    let temp = tempfile::tempdir().expect("tempdir");
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "core.autocrlf", "false"]);
    git(
        temp.path(),
        &["config", "user.email", "verify@example.test"],
    );
    git(temp.path(), &["config", "user.name", "Verifier"]);
    std::fs::write(temp.path().join("old.txt"), "old\n").expect("write old");
    std::fs::write(temp.path().join("delete.txt"), "delete\n").expect("write delete");
    std::fs::write(temp.path().join("source.txt"), "source\n").expect("write source");
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-q", "-m", "base"]);
    let base = git_text(temp.path(), &["rev-parse", "HEAD"]);
    std::fs::rename(temp.path().join("old.txt"), temp.path().join("new.txt")).expect("rename");
    std::fs::remove_file(temp.path().join("delete.txt")).expect("delete");
    std::fs::copy(
        temp.path().join("source.txt"),
        temp.path().join("copy.txt"),
    )
    .expect("copy");
    let worktree = RepositorySnapshot::from_worktree(temp.path()).expect("worktree snapshot");
    assert!(
        worktree
            .records
            .iter()
            .any(|record| record.path.as_utf8() == Some("new.txt"))
    );
    git(temp.path(), &["add", "-A"]);
    git(temp.path(), &["commit", "-q", "-m", "head"]);
    let head = git_text(temp.path(), &["rev-parse", "HEAD"]);
    let diff = RepositorySnapshot::from_commit_diff(
        temp.path(),
        &base,
        &head,
        CommitComparisonMode::Direct,
    )
    .expect("commit diff");
    assert!(
        diff.records
            .iter()
            .any(|record| record.path.as_utf8() == Some("new.txt"))
    );
    assert!(
        diff.records
            .iter()
            .any(|record| record.path.as_utf8() == Some("delete.txt"))
    );
    assert!(diff.records.iter().any(|record| {
        record.status == "A" && record.path.as_utf8() == Some("copy.txt")
    }));
}

#[test]
fn worktree_uses_the_canonical_git_top_level_from_a_nested_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "core.autocrlf", "false"]);
    let nested = temp.path().join("nested/deeper");
    std::fs::create_dir_all(&nested).expect("nested");
    std::fs::write(temp.path().join("changed.txt"), "changed\n").expect("write");
    let snapshot = RepositorySnapshot::from_worktree(&nested).expect("worktree snapshot");
    assert_eq!(
        snapshot.repository_root,
        Some(std::fs::canonicalize(temp.path()).expect("canonical root"))
    );
    assert_eq!(snapshot.records[0].path.as_utf8(), Some("changed.txt"));
}

#[test]
fn commit_diff_fallback_preserves_mode_and_validated_ids() {
    let temp = tempfile::tempdir().expect("tempdir");
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "user.email", "verify@example.test"]);
    git(temp.path(), &["config", "user.name", "Verifier"]);
    std::fs::write(temp.path().join("base.txt"), "base\n").expect("write");
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-q", "-m", "base"]);
    let oid = git_text(temp.path(), &["rev-parse", "HEAD"]);
    let snapshot = RepositorySnapshot::commit_diff_fallback(
        temp.path(),
        &oid,
        "not-a-commit",
        CommitComparisonMode::PullRequestMergeBase,
        "missing shallow history",
    );
    assert!(matches!(
        snapshot.source,
        SnapshotSource::CommitDiff {
            base: Some(ref base),
            head: None,
            merge_base: None,
            pull_request: true,
        } if base == &oid
    ));
    assert!(!snapshot.complete);
}

#[test]
fn pull_request_snapshot_records_the_validated_merge_base() {
    let temp = tempfile::tempdir().expect("tempdir");
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "core.autocrlf", "false"]);
    git(temp.path(), &["config", "user.email", "verify@example.test"]);
    git(temp.path(), &["config", "user.name", "Verifier"]);
    std::fs::write(temp.path().join("base.txt"), "base\n").expect("base");
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-q", "-m", "base"]);
    let merge_base = git_text(temp.path(), &["rev-parse", "HEAD"]);
    let main_branch = git_text(temp.path(), &["branch", "--show-current"]);
    git(temp.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(temp.path().join("feature.txt"), "feature\n").expect("feature");
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-q", "-m", "feature"]);
    let head = git_text(temp.path(), &["rev-parse", "HEAD"]);
    git(temp.path(), &["checkout", "-q", &main_branch]);
    std::fs::write(temp.path().join("main.txt"), "main\n").expect("main");
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-q", "-m", "main"]);
    let base = git_text(temp.path(), &["rev-parse", "HEAD"]);
    let snapshot = RepositorySnapshot::from_commit_diff(
        temp.path(),
        &base,
        &head,
        CommitComparisonMode::PullRequestMergeBase,
    )
    .expect("pull request snapshot");
    assert!(matches!(
        snapshot.source,
        SnapshotSource::CommitDiff {
            base: Some(ref resolved_base),
            head: Some(ref resolved_head),
            merge_base: Some(ref resolved_merge_base),
            pull_request: true,
        } if resolved_base == &base
            && resolved_head == &head
            && resolved_merge_base == &merge_base
    ));
    assert!(snapshot.records.iter().any(|record| {
        record.status == "A" && record.path.as_utf8() == Some("feature.txt")
    }));
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

fn git_text(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("utf8 oid")
        .trim()
        .to_string()
}
