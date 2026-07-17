use super::*;

#[tokio::test]
async fn active_output_file_lock_blocks_removal_until_release() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("active.log");
    tokio::fs::write(&path, b"active")
        .await
        .expect("write active artifact");
    let active = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open active artifact");
    active.try_lock().expect("lock active artifact");

    assert!(!remove_inactive_output_path(path.clone()).await);
    assert!(path.exists());

    drop(active);
    assert!(remove_inactive_output_path(path.clone()).await);
    assert!(!path.exists());
}

#[tokio::test]
async fn replacement_does_not_truncate_before_acquiring_the_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_raw_output_artifact(temp.path(), "thread", b"retained output").await;
    let RawOutputArtifact::Stored { path, .. } = &artifact else {
        panic!("expected stored artifact");
    };
    let active = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open active artifact");
    active.try_lock().expect("lock active artifact");

    let replaced = replace_raw_output_artifact(&artifact, b"replacement").await;

    assert!(matches!(replaced, RawOutputArtifact::Failed { .. }));
    drop(active);
    assert_eq!(
        tokio::fs::read(path).await.expect("read retained artifact"),
        b"retained output"
    );
}

#[tokio::test]
async fn per_thread_retention_skips_active_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output").join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    let active_path = directory.join("0000.log");
    tokio::fs::write(&active_path, b"active")
        .await
        .expect("active artifact");
    let active = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&active_path)
        .expect("open active artifact");
    active.try_lock().expect("lock active artifact");
    for index in 1..=(max_retained_artifacts_per_thread() + 2) {
        tokio::fs::write(directory.join(format!("{index:04}.log")), b"inactive")
            .await
            .expect("inactive artifact");
    }
    let keep_path = directory.join(format!(
        "{:04}.log",
        max_retained_artifacts_per_thread() + 2
    ));

    enforce_retention(&directory, &keep_path);
    wait_for_retention_idle().await;

    assert!(active_path.exists());
    assert!(keep_path.exists());
    let mut entries = tokio::fs::read_dir(&directory)
        .await
        .expect("read artifact directory");
    let mut count = 0;
    while entries
        .next_entry()
        .await
        .expect("read artifact entry")
        .is_some()
    {
        count += 1;
    }
    assert_eq!(count, max_retained_artifacts_per_thread());
    drop(active);
}

#[tokio::test]
async fn global_retention_bounds_artifacts_across_threads() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("tool-output");
    let total = max_retained_artifacts_total() + 5;
    for index in 0..total {
        let directory = root.join(format!("thread-{}", index % 4));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("thread directory");
        tokio::fs::write(directory.join(format!("{index:04}.log")), b"artifact")
            .await
            .expect("artifact");
    }
    let keep_path = root.join("thread-0").join("keep.log");
    tokio::fs::write(&keep_path, b"keep")
        .await
        .expect("keep artifact");

    enforce_global_retention(&root, &keep_path).await;

    let mut retained = 0;
    let mut thread_directories = tokio::fs::read_dir(&root).await.expect("tool output root");
    while let Some(thread) = thread_directories
        .next_entry()
        .await
        .expect("thread directory")
    {
        let mut entries = tokio::fs::read_dir(thread.path())
            .await
            .expect("thread artifacts");
        while entries
            .next_entry()
            .await
            .expect("artifact entry")
            .is_some()
        {
            retained += 1;
        }
    }
    assert_eq!(retained, max_retained_artifacts_total());
    assert!(keep_path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creation_enforces_the_per_thread_cap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().to_path_buf();
    let mut creates = Vec::new();
    for index in 0..(max_retained_artifacts_per_thread() + 12) {
        let home = home.clone();
        creates.push(tokio::spawn(async move {
            create_raw_output_artifact(&home, "thread", format!("artifact-{index}").as_bytes())
                .await
        }));
    }
    for create in creates {
        assert!(matches!(
            create.await.expect("artifact creation task"),
            RawOutputArtifact::Stored { .. }
        ));
    }
    wait_for_retention_idle().await;

    let directory = home.join("tool-output").join("thread");
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .expect("read artifact directory");
    let mut count = 0;
    while entries
        .next_entry()
        .await
        .expect("read artifact entry")
        .is_some()
    {
        count += 1;
    }
    assert_eq!(count, max_retained_artifacts_per_thread());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_roots_do_not_share_a_retention_wait() {
    let temp = tempfile::tempdir().expect("tempdir");
    let blocked_home = temp.path().join("blocked-home");
    let free_home = temp.path().join("free-home");
    let blocked_directory = blocked_home.join("tool-output").join("thread");
    tokio::fs::create_dir_all(&blocked_directory)
        .await
        .expect("artifact directory");
    let blocked_lock = retention_directory_lock(&blocked_directory);
    let blocked_guard = blocked_lock.lock().await;
    let (blocked_artifact, free_artifact) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            tokio::join!(
                create_raw_output_artifact(&blocked_home, "thread", b"blocked"),
                create_raw_output_artifact(&free_home, "thread", b"free")
            )
        },
    )
    .await
    .expect("artifact creation must not wait for retention sweeps");
    assert!(matches!(blocked_artifact, RawOutputArtifact::Stored { .. }));
    assert!(matches!(free_artifact, RawOutputArtifact::Stored { .. }));

    let free_directory = free_home.join("tool-output").join("thread");
    let free_root = free_home.join("tool-output");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let free_root_reconciled = {
                let pending = retention_janitor()
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                !pending.directories.contains_key(&free_directory)
                    && !pending.roots.contains_key(&free_root)
            };
            if free_root_reconciled {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a busy directory must not stall reconciliation for another root");

    drop(blocked_guard);
    wait_for_retention_idle().await;
}

#[tokio::test]
async fn local_retention_retries_locked_overage_until_the_cap_is_met() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output").join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    let mut active_files = Vec::new();
    for index in 0..=max_retained_artifacts_per_thread() {
        let path = directory.join(format!("{index:04}.log"));
        tokio::fs::write(&path, b"active")
            .await
            .expect("active artifact");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open active artifact");
        file.try_lock().expect("lock active artifact");
        active_files.push(file);
    }
    let keep_path = directory.join(format!(
        "{:04}.log",
        max_retained_artifacts_per_thread()
    ));

    enforce_retention(&directory, &keep_path);
    drop(active_files.remove(0));
    wait_for_retention_idle().await;

    let mut entries = tokio::fs::read_dir(&directory)
        .await
        .expect("read artifact directory");
    let mut count = 0;
    while entries
        .next_entry()
        .await
        .expect("read artifact entry")
        .is_some()
    {
        count += 1;
    }
    assert_eq!(count, max_retained_artifacts_per_thread());
}
