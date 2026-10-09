use super::*;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::test]
async fn bounded_reads_preserve_limits_and_confined_results() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("file");
    let uri = PathUri::from_host_native_path(&path)?;
    let root = PathUri::from_host_native_path(directory.path())?;
    let fs = LocalFileSystem::unsandboxed();
    for size in [
        0usize,
        1,
        FILE_READ_CHUNK_SIZE,
        3 * FILE_READ_CHUNK_SIZE + 17,
    ] {
        let bytes = (0..size).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        std::fs::write(&path, &bytes)?;
        for max_bytes in [size.saturating_sub(1), size, size + 1] {
            let expected = (size <= max_bytes).then_some(bytes.clone());
            assert_eq!(fs.read_file_bounded(&uri, max_bytes, None).await?, expected);
            assert_eq!(
                fs.read_file_bounded_confined(&uri, &root, max_bytes, None)
                    .await?,
                expected
            );
        }
    }
    let outside = tempfile::NamedTempFile::new()?;
    let outside_uri = PathUri::from_host_native_path(outside.path())?;
    assert_eq!(
        fs.read_file_bounded_confined(&outside_uri, &root, 10, None)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    Ok(())
}

#[test]
fn bounded_read_verifies_content_across_two_passes_and_final_open() -> io::Result<()> {
    for changed in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("file");
        std::fs::write(&path, b"original")?;
        let modified = std::fs::metadata(&path)?.modified()?;
        let mut opens = 0;
        let bytes = read_bounded_file_sync(
            || {
                opens += 1;
                if changed && opens == 2 {
                    std::fs::write(&path, b"modified")?;
                    std::fs::File::options()
                        .write(true)
                        .open(&path)?
                        .set_modified(modified)?;
                }
                regular_file::open_sync(&path)
            },
            8,
            &CancellationToken::new(),
        )?;
        assert_eq!(bytes, (!changed).then(|| b"original".to_vec()));
        assert_eq!(opens, 3);
    }
    Ok(())
}

#[test_case::test_case(2; "second_open")]
#[test_case::test_case(3; "final_open")]
fn bounded_read_detects_same_content_replacement(replace_on: usize) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("file");
    let replacement = directory.path().join("replacement");
    std::fs::write(&path, b"same")?;
    std::fs::write(&replacement, b"same")?;
    let mut opens = 0;
    let bytes = read_bounded_file_sync(
        || {
            opens += 1;
            if opens == replace_on {
                std::fs::rename(&replacement, &path)?;
            }
            regular_file::open_sync(&path)
        },
        4,
        &CancellationToken::new(),
    )?;
    assert_eq!(bytes, None);
    assert_eq!(opens, 3);
    Ok(())
}

#[test]
fn bounded_chunks_preserve_late_errors_and_stop_at_limit() {
    struct Fails;
    impl Read for Fails {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("late read error"))
        }
    }
    let cancel = CancellationToken::new();
    let mut matched = true;
    let error = read_bounded_chunks(
        &mut Read::chain(std::io::Cursor::new(b"changed"), Fails),
        8,
        20,
        &cancel,
        |chunk| matched &= chunk == b"original",
    )
    .unwrap_err();
    assert!(!matched);
    assert_eq!(error.to_string(), "late read error");
    let mut reader = std::io::Cursor::new(vec![0; 100]);
    assert_eq!(
        read_bounded_chunks(&mut reader, 100, 4, &cancel, |_| {}).unwrap(),
        None
    );
    assert_eq!(reader.position(), 5);
}

#[test]
fn bounded_chunks_check_cancellation_between_chunks() {
    let cancel = CancellationToken::new();
    let mut reader = std::io::Cursor::new(vec![0; 3 * FILE_READ_CHUNK_SIZE]);
    let error = read_bounded_chunks(
        &mut reader,
        FILE_READ_CHUNK_SIZE,
        usize::MAX,
        &cancel,
        |_| cancel.cancel(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(reader.position(), FILE_READ_CHUNK_SIZE as u64);
}

#[tokio::test]
async fn dropped_enumeration_stops_before_the_next_entry() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    for name in ["a", "b", "c"] {
        std::fs::write(directory.path().join(name), b"")?;
    }
    let path = directory.path().to_path_buf();
    let examined = Arc::new(AtomicUsize::new(0));
    let worker_examined = Arc::clone(&examined);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(run_cancellable_file_system_task(move |cancel| {
        let mut started_tx = Some(started_tx);
        let result = read_directory_sync(&path, Some(100), &cancel, |name, metadata, link| {
            worker_examined.fetch_add(1, Ordering::SeqCst);
            if let Some(started_tx) = started_tx.take() {
                let _ = started_tx.send(cancel.clone());
                let _ = resume_rx.recv_timeout(Duration::from_secs(5));
            }
            directory_entry(name, metadata, link)
        });
        let _ = finished_tx.send(result);
        Ok(())
    }));
    let cancel = tokio::time::timeout(Duration::from_secs(5), started_rx).await??;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(cancel.is_cancelled());
    resume_tx.send(())?;
    let result = tokio::time::timeout(Duration::from_secs(5), finished_rx).await??;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    assert_eq!(examined.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn native_walk_metadata_preserves_link_kinds_and_raw_budgets() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("root");
    std::fs::create_dir(&root)?;
    let target = directory.path().join("target");
    std::fs::create_dir(&target)?;
    std::fs::write(target.join("child"), b"child")?;
    std::fs::write(root.join("file"), b"file")?;
    std::os::windows::fs::symlink_dir(&target, root.join("directory-link"))?;
    std::os::windows::fs::symlink_file(target.join("child"), root.join("file-link"))?;
    std::os::windows::fs::symlink_file(target.join("missing"), root.join("dangling"))?;
    let uri = PathUri::from_host_native_path(&root)?;
    let batch = DirectFileSystem
        .read_directory_bounded_for_walk(&uri, 4, None)
        .await?;
    assert_eq!(batch.entries_examined, 4);
    assert!(batch.limit_reached);
    assert_eq!(batch.entries.len(), 3);
    for entry in batch.entries {
        let metadata = entry
            .metadata
            .expect("native enumeration should supply metadata");
        assert_eq!(metadata.is_symlink, entry.file_name.ends_with("-link"));
        assert_eq!(metadata.is_directory, entry.file_name == "directory-link");
        assert_eq!(metadata.is_file, entry.file_name != "directory-link");
    }
    let options = WalkOptions {
        max_depth: 4,
        max_directories: 10,
        max_entries: 10,
        follow_directory_symlinks: false,
        prune_hidden_directories: false,
        filters: Default::default(),
    };
    let fs = LocalFileSystem::unsandboxed();
    let walk = fs.walk(&uri, options.clone(), None).await?;
    assert_eq!(
        walk.entries
            .iter()
            .map(|entry| entry.path.basename().unwrap())
            .collect::<Vec<_>>(),
        ["file", "file-link"]
    );
    assert!(!walk.truncated);
    assert!(walk.errors.is_empty());
    let walk = fs
        .walk(
            &uri,
            WalkOptions {
                follow_directory_symlinks: true,
                ..options
            },
            None,
        )
        .await?;
    assert_eq!(walk.entries.len(), 4);
    assert!(
        walk.entries
            .iter()
            .any(|entry| entry.path == uri.join("directory-link/child").unwrap())
    );
    assert!(!walk.truncated);
    assert!(walk.errors.is_empty());
    Ok(())
}
