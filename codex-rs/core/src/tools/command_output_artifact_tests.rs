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

    enforce_retention(&directory, &keep_path).await;

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

#[tokio::test]
async fn protected_evidence_artifact_survives_per_thread_retention_without_reducing_generic_limit()
{
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_evidence_output_artifact(temp.path(), "thread", b"durable evidence")
        .await
        .expect("pending evidence")
        .mark_durable();
    let RawOutputArtifact::Stored { path, .. } = &artifact else {
        panic!("expected stored artifact");
    };

    for index in 0..(max_retained_artifacts_per_thread() + 5) {
        create_raw_output_artifact(temp.path(), "thread", format!("generic-{index}").as_bytes())
            .await;
    }

    assert!(path.exists());
    assert!(evidence_protection_path(path).exists());
    let mut generic_logs = 0;
    let mut total_logs = 0;
    let mut entries = tokio::fs::read_dir(path.parent().expect("thread directory"))
        .await
        .expect("thread artifacts");
    while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            total_logs += 1;
            if entry.path() != *path {
                generic_logs += 1;
            }
        }
    }
    assert_eq!(generic_logs, max_retained_artifacts_per_thread());
    assert_eq!(total_logs, max_retained_artifacts_per_thread() + 1);
}

#[tokio::test]
async fn evidence_creation_holds_retention_permit_until_marker_is_durable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let evidence_barrier = Arc::clone(&barrier);
    let codex_home = temp.path().to_path_buf();
    let evidence_task = tokio::spawn(async move {
        create_evidence_output_artifact_inner(
            &codex_home,
            "thread",
            b"durable evidence",
            Some(evidence_barrier.as_ref()),
        )
        .await
    });

    barrier.wait().await;
    let directory = temp.path().join("tool-output").join("thread");
    let mut entries = tokio::fs::read_dir(&directory)
        .await
        .expect("thread artifacts");
    let log_path = loop {
        let entry = entries
            .next_entry()
            .await
            .expect("artifact entry")
            .expect("evidence log");
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            break entry.path();
        }
    };
    assert_eq!(
        tokio::fs::symlink_metadata(&log_path)
            .await
            .expect("synced evidence log")
            .len(),
        b"durable evidence".len() as u64
    );
    assert!(!evidence_protection_path(&log_path).exists());

    let mut churn = tokio::task::JoinSet::new();
    for index in 0..(max_retained_artifacts_per_thread() + 1) {
        let codex_home = temp.path().to_path_buf();
        churn.spawn(async move {
            create_raw_output_artifact(&codex_home, "thread", format!("generic-{index}").as_bytes())
                .await
        });
    }

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let mut log_count = 0;
            let mut entries = tokio::fs::read_dir(&directory)
                .await
                .expect("thread artifacts");
            while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
                if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
                    log_count += 1;
                }
            }
            if log_count == max_retained_artifacts_per_thread() + 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generic artifacts should reach the retention gate");

    barrier.wait().await;
    let artifact = evidence_task
        .await
        .expect("evidence creation task")
        .expect("pending evidence")
        .mark_durable();
    while let Some(result) = churn.join_next().await {
        result.expect("generic artifact task");
    }

    let RawOutputArtifact::Stored { path, .. } = artifact else {
        panic!("expected protected evidence artifact");
    };
    assert!(path.exists());
    assert!(evidence_protection_path(&path).exists());
}

#[tokio::test]
async fn active_reader_trim_unprotects_artifact_before_eventual_retention_cleanup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = create_evidence_output_artifact(temp.path(), "thread", b"durable evidence")
        .await
        .expect("pending evidence")
        .mark_durable();
    let RawOutputArtifact::Stored { id, path, .. } = &artifact else {
        panic!("expected stored evidence");
    };
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open evidence reader");
    reader.try_lock().expect("lock evidence reader");

    assert!(
        delete_evidence_artifact(temp.path(), "thread", &id.to_string())
            .await
            .is_err()
    );
    assert!(path.exists());
    assert!(!evidence_protection_path(path).exists());
    drop(reader);

    for index in 0..(max_retained_artifacts_per_thread() + 5) {
        create_raw_output_artifact(temp.path(), "thread", format!("generic-{index}").as_bytes())
            .await;
    }
    assert!(!path.exists());
}

#[tokio::test]
async fn cancelled_evidence_creation_leaves_no_protected_orphan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let creation_barrier = Arc::clone(&barrier);
    let codex_home = temp.path().to_path_buf();
    let creation_task = tokio::spawn(async move {
        create_evidence_output_artifact_inner(
            &codex_home,
            "thread",
            b"durable evidence",
            Some(creation_barrier.as_ref()),
        )
        .await
    });
    barrier.wait().await;
    creation_task.abort();
    assert!(matches!(
        creation_task.await,
        Err(err) if err.is_cancelled()
    ));

    let directory = temp.path().join("tool-output").join("thread");
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .expect("thread artifacts");
    while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
        let path = entry.path();
        let extension = path.extension().and_then(|value| value.to_str());
        assert_ne!(extension, Some("log"));
        assert_ne!(extension, Some(EVIDENCE_PROTECTION_EXTENSION));
    }
}

#[tokio::test]
async fn cancelled_pending_evidence_lease_cleans_up_but_durable_lease_survives() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pending = create_evidence_output_artifact(temp.path(), "thread", b"pending evidence")
        .await
        .expect("pending evidence");
    let pending_path = pending.path.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let persistence_barrier = Arc::clone(&barrier);
    let persistence_task = tokio::spawn(async move {
        persistence_barrier.wait().await;
        persistence_barrier.wait().await;
        pending.mark_durable()
    });
    barrier.wait().await;
    persistence_task.abort();
    assert!(
        persistence_task
            .await
            .expect_err("persistence should be cancelled")
            .is_cancelled()
    );
    assert!(!pending_path.exists());
    assert!(!evidence_protection_path(&pending_path).exists());

    let durable = create_evidence_output_artifact(temp.path(), "thread", b"durable evidence")
        .await
        .expect("pending durable evidence")
        .mark_durable();
    let RawOutputArtifact::Stored { path, .. } = durable else {
        panic!("expected durable evidence");
    };
    assert!(path.exists());
    assert!(evidence_protection_path(&path).exists());
}

#[tokio::test]
async fn global_retention_skips_protected_evidence_without_broadening_generic_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact =
        create_evidence_output_artifact(temp.path(), "evidence-thread", b"durable evidence")
            .await
            .expect("pending evidence")
            .mark_durable();
    let RawOutputArtifact::Stored {
        path: evidence_path,
        ..
    } = &artifact
    else {
        panic!("expected stored artifact");
    };
    let root = temp.path().join("tool-output");
    for index in 0..(max_retained_artifacts_total() + 5) {
        let directory = root.join(format!("generic-thread-{}", index % 16));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("thread directory");
        tokio::fs::write(directory.join(format!("{index:04}.log")), b"generic")
            .await
            .expect("generic artifact");
    }
    let keep_path = root.join("generic-thread-0").join("keep.log");
    tokio::fs::write(&keep_path, b"keep")
        .await
        .expect("keep artifact");

    enforce_global_retention(&root, &keep_path).await;

    assert!(evidence_path.exists());
    let mut generic_logs = 0;
    let mut directories = tokio::fs::read_dir(&root).await.expect("tool output root");
    while let Some(directory) = directories.next_entry().await.expect("thread directory") {
        let mut entries = tokio::fs::read_dir(directory.path())
            .await
            .expect("thread artifacts");
        while let Some(entry) = entries.next_entry().await.expect("artifact entry") {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("log")
                && entry.path() != *evidence_path
            {
                generic_logs += 1;
            }
        }
    }
    assert_eq!(generic_logs, max_retained_artifacts_total());
}

#[tokio::test]
async fn retention_sweeps_are_serialized() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("tool-output").join("thread");
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("artifact directory");
    let keep_path = directory.join("keep.log");
    tokio::fs::write(&keep_path, b"keep")
        .await
        .expect("keep artifact");
    let retention_permit = retention_sweep_permit().await;
    let mut sweep = tokio::spawn({
        let directory = directory.clone();
        let keep_path = keep_path.clone();
        async move { enforce_retention(&directory, &keep_path).await }
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut sweep)
            .await
            .is_err(),
        "a concurrent sweep must wait for the process-wide retention lock"
    );
    drop(retention_permit);
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut sweep)
        .await
        .expect("retention sweep should resume after lock release")
        .expect("retention sweep task");
}
