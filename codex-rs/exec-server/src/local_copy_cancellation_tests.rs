use super::*;
use pretty_assertions::assert_eq;

#[test]
fn cancelled_recursive_copy_preserves_existing_destination() -> io::Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("source");
    let target = home.path().join("target");
    std::fs::create_dir(&source)?;
    std::fs::create_dir(&target)?;
    std::fs::write(source.join("file"), b"new")?;
    std::fs::write(target.join("file"), b"old")?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = copy_dir_recursive(&source, &target, &source.canonicalize()?, &cancel)
        .expect_err("cancelled");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(std::fs::read(target.join("file"))?, b"old");
    Ok(())
}

#[test]
fn recursive_copy_failure_retains_effect_prefix_and_current_target_uncertainty() -> io::Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("source");
    let target = home.path().join("target");
    std::fs::create_dir(&source)?;
    std::fs::write(source.join("file"), b"new contents")?;
    // A directory at the file destination guarantees a copy error after the
    // destination root was created; no enumeration-order assumption is needed.
    std::fs::create_dir_all(target.join("file"))?;
    let error = copy_dir_recursive(
        &source,
        &target,
        &source.canonicalize()?,
        &CancellationToken::new(),
    )
    .unwrap_err();
    let progress = codex_file_system::CopyFailure::from_io_error(&error).unwrap();
    assert!(progress.target_may_have_changed);
    assert_eq!(progress.completed_entries, 0);
    assert!(progress.to_string().contains("no rollback"));
    assert!(target.join("file").is_dir());
    let cancelled_target = home.path().join("cancelled");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = copy_dir_recursive(&source, &cancelled_target, &source.canonicalize()?, &cancel)
        .unwrap_err();
    let progress = codex_file_system::CopyFailure::from_io_error(&error).unwrap();
    assert_eq!(progress.completed_entries, 0);
    assert!(!progress.target_may_have_changed);
    assert!(!cancelled_target.exists());
    Ok(())
}

#[tokio::test]
async fn dropping_copy_worker_stops_before_next_directory() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("source");
    let target = home.path().join("target");
    std::fs::create_dir(&source)?;
    std::fs::write(source.join("file"), b"contents")?;
    let source_root = source.canonicalize()?;
    let destination = target.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(run_cancellable_file_system_task(move |cancel| {
        let _ = started_tx.send(());
        resume_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("resume");
        let result = copy_dir_recursive(&source, &destination, &source_root, &cancel);
        let _ = finished_tx.send(result.as_ref().err().map(io::Error::kind));
        result
    }));
    tokio::time::timeout(std::time::Duration::from_secs(5), started_rx).await??;
    task.abort();
    assert!(task.await.expect_err("aborted").is_cancelled());
    resume_tx.send(())?;
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), finished_rx).await??,
        Some(io::ErrorKind::Interrupted)
    );
    assert!(!target.exists());
    Ok(())
}

#[tokio::test]
async fn recursive_copy_still_copies_nested_contents() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("source");
    let target = home.path().join("target");
    std::fs::create_dir_all(source.join("nested"))?;
    std::fs::write(source.join("nested/file"), b"contents")?;
    LocalFileSystem::unsandboxed()
        .copy(
            &PathUri::from_host_native_path(source)?,
            &PathUri::from_host_native_path(&target)?,
            CopyOptions { recursive: true },
            None,
        )
        .await?;
    assert_eq!(std::fs::read(target.join("nested/file"))?, b"contents");
    Ok(())
}
