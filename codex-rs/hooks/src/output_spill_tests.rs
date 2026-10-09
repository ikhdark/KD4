use super::*;
use anyhow::Context;
use anyhow::Result;
use std::fs::FileTimes;
use std::time::SystemTime;
use tempfile::tempdir;

fn write_spill(path: &Path, text: &str, modified: SystemTime) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("spill parent")?)?;
    std::fs::write(path, text)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_times(FileTimes::new().set_modified(modified))?;
    Ok(())
}

fn test_policy(
    max_age: Duration,
    active_grace: Duration,
    max_files: usize,
    max_bytes: u64,
) -> SpillRetentionPolicy {
    SpillRetentionPolicy {
        max_age,
        active_grace,
        max_files,
        max_bytes,
    }
}

// The previous per-operation Tokio scan is retained only as a behavioral and
// performance reference. Both collectors must inspect the same complete tree.
async fn collect_spill_files_async_reference(output_dir: &Path) -> std::io::Result<Vec<SpillFile>> {
    let mut thread_dirs = match fs::read_dir(output_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut files = Vec::new();
    while let Some(thread_entry) = ignore_disappeared(thread_dirs.next_entry().await)?.flatten() {
        if !ignore_disappeared(thread_entry.file_type().await)?.is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(mut thread_files) = ignore_disappeared(fs::read_dir(thread_entry.path()).await)?
        else {
            continue;
        };
        while let Some(file_entry) = ignore_disappeared(thread_files.next_entry().await)?.flatten()
        {
            if !ignore_disappeared(file_entry.file_type().await)?.is_some_and(|kind| kind.is_file())
                || file_entry
                    .path()
                    .extension()
                    .and_then(|value| value.to_str())
                    != Some("txt")
            {
                continue;
            }
            let Some(metadata) = ignore_disappeared(file_entry.metadata().await)? else {
                continue;
            };
            files.push(SpillFile {
                path: file_entry.path(),
                modified: metadata.modified()?,
                len: metadata.len(),
            });
        }
    }
    Ok(files)
}

fn sorted_spill_records(files: Vec<SpillFile>) -> Vec<(PathBuf, SystemTime, u64)> {
    let mut records = files
        .into_iter()
        .map(|file| (file.path, file.modified, file.len))
        .collect::<Vec<_>>();
    records.sort();
    records
}

#[tokio::test]
async fn spill_scan_preserves_complete_metadata_and_filtering() -> Result<()> {
    let dir = tempdir()?;
    for (name, text) in [
        ("one/a.txt", "a"),
        ("two/b.txt", "bbb"),
        ("two/skip.json", "skip"),
        ("root.txt", "skip"),
    ] {
        write_spill(&dir.path().join(name), text, SystemTime::UNIX_EPOCH)?;
    }
    std::fs::create_dir_all(dir.path().join("one/directory.txt"))?;
    let actual = sorted_spill_records(collect_spill_files(dir.path()).await?);
    assert_eq!(
        actual,
        vec![
            (dir.path().join("one/a.txt"), SystemTime::UNIX_EPOCH, 1),
            (dir.path().join("two/b.txt"), SystemTime::UNIX_EPOCH, 3),
        ]
    );
    assert_eq!(
        actual,
        sorted_spill_records(collect_spill_files_async_reference(dir.path()).await?)
    );
    assert!(
        collect_spill_files(&dir.path().join("missing"))
            .await?
            .is_empty()
    );
    assert!(
        collect_spill_files(&dir.path().join("root.txt"))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "manual performance measurement; no wall-clock assertion"]
async fn benchmark_spill_scan() -> Result<()> {
    let dir = tempdir()?;
    for index in 0..512 {
        write_spill(
            &dir.path()
                .join(format!("thread-{}/out-{index}.txt", index % 8)),
            "retained output",
            SystemTime::UNIX_EPOCH,
        )?;
    }
    let mut async_us = Vec::new();
    let mut blocking_us = Vec::new();
    for round in 0..12 {
        for blocking in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let started = Instant::now();
            let files = if blocking {
                collect_spill_files(dir.path()).await?
            } else {
                collect_spill_files_async_reference(dir.path()).await?
            };
            let elapsed = started.elapsed().as_micros();
            assert_eq!(files.len(), 512);
            if blocking {
                blocking_us.push(elapsed);
            } else {
                async_us.push(elapsed);
            }
        }
    }
    async_us.sort_unstable();
    blocking_us.sort_unstable();
    eprintln!(
        "spill scan: files=512 directories=8 samples=12 async_median_us={} blocking_median_us={}",
        async_us[6], blocking_us[6]
    );
    Ok(())
}

#[tokio::test]
async fn small_hook_output_remains_inline() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let thread_id = ThreadId::new();
    let spiller = HookOutputSpiller {
        output_dir: output_dir.clone(),
        last_prune: Arc::default(),
    };

    let output = spiller
        .maybe_spill_text(thread_id, "short".to_string())
        .await;

    assert_eq!(output, "short");
    assert!(!output_dir.exists());
    Ok(())
}

#[tokio::test]
async fn large_hook_output_spills_to_file() -> Result<()> {
    let dir = tempdir()?;
    let text = "hook output ".repeat(1_000);
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller {
        output_dir,
        last_prune: Arc::default(),
    };

    let output = spiller
        .maybe_spill_text(ThreadId::new(), text.clone())
        .await;

    assert!(output.contains("[omitted before retained middle]"));
    assert!(output.contains("[omitted after retained middle]"));
    assert!(approx_token_count(&output) <= HOOK_OUTPUT_TOKEN_LIMIT);
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("Full hook output saved to: "))
        .context("spill path")?;
    assert_eq!(fs::read_to_string(path).await?, text);
    Ok(())
}

#[tokio::test]
async fn spill_batches_throttle_cleanup_and_keep_writer_directories() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller {
        output_dir: output_dir.clone(),
        last_prune: Arc::default(),
    };
    let thread_id = ThreadId::new();
    let expired = output_dir.join(thread_id.to_string()).join("expired.txt");
    write_spill(expired.as_ref(), "old", SystemTime::UNIX_EPOCH)?;
    let text = "output ".repeat(2000);
    let outputs = spiller
        .maybe_spill_texts(thread_id, vec![text.clone(), text.clone()])
        .await;
    assert!(!expired.exists());
    assert!(expired.parent().context("parent")?.exists());
    assert_eq!(outputs.len(), 2);
    for output in outputs {
        let path = output
            .lines()
            .find_map(|line| line.strip_prefix("Full hook output saved to: "))
            .context("spill path")?;
        assert_eq!(fs::read_to_string(path).await?, text);
        assert!(approx_token_count(&output) <= HOOK_OUTPUT_TOKEN_LIMIT);
    }
    write_spill(expired.as_ref(), "old", SystemTime::UNIX_EPOCH)?;
    let output = spiller.clone().maybe_spill_text(thread_id, text).await;
    assert!(output.contains("Full hook output saved to:"));
    assert!(
        expired.exists(),
        "a second sweep ran inside the throttle interval"
    );
    *spiller.last_prune.lock().await = Some(Instant::now() - Duration::from_secs(61));
    spiller.prune_crash_leftovers(None).await;
    assert!(
        !expired.exists(),
        "cleanup did not resume after the throttle interval"
    );
    Ok(())
}

#[tokio::test]
async fn cleanup_keeps_empty_directories_available_to_writers() -> Result<()> {
    let dir = tempdir()?;
    let thread_dir = dir.path().join("thread");
    let expired = thread_dir.join("expired.txt");
    write_spill(&expired, "old", SystemTime::UNIX_EPOCH)?;
    prune_crash_leftovers_at(dir.path(), None, SPILL_RETENTION_POLICY, SystemTime::now()).await?;
    assert!(!expired.exists());
    let pending_write = thread_dir.join("new.txt");
    fs::write(&pending_write, "full output").await?;
    assert_eq!(fs::read_to_string(pending_write).await?, "full output");
    Ok(())
}

#[tokio::test]
async fn output_spill_prunes_expired_crash_leftovers() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = dir.path().join(HOOK_OUTPUTS_DIR);
    let thread_dir = output_dir.join(ThreadId::new().to_string());
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
    let expired = thread_dir.join("expired.txt");
    let current = thread_dir.join("current.txt");
    write_spill(
        &expired,
        "expired",
        now.checked_sub(Duration::from_secs(2 * 24 * 60 * 60))
            .context("expired time")?,
    )?;
    write_spill(&current, "current", now)?;

    prune_crash_leftovers_at(
        &output_dir,
        None,
        test_policy(
            Duration::from_secs(24 * 60 * 60),
            Duration::from_secs(60 * 60),
            usize::MAX,
            u64::MAX,
        ),
        now,
    )
    .await?;

    assert!(!expired.exists());
    assert!(current.exists());
    Ok(())
}

#[tokio::test]
async fn output_spill_preserves_current_files_inside_active_grace() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = dir.path().join(HOOK_OUTPUTS_DIR);
    let current = output_dir
        .join(ThreadId::new().to_string())
        .join("current.txt");
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
    write_spill(&current, "current", now)?;

    prune_crash_leftovers_at(
        &output_dir,
        None,
        test_policy(
            Duration::from_secs(24 * 60 * 60),
            Duration::from_secs(60 * 60),
            0,
            0,
        ),
        now,
    )
    .await?;

    assert!(current.exists());
    Ok(())
}

#[tokio::test]
async fn output_spill_quota_prunes_oldest_crash_leftovers_by_count_and_bytes() -> Result<()> {
    for (max_files, max_bytes, first_retained) in [(3, u64::MAX, 2), (usize::MAX, 16, 3)] {
        let dir = tempdir()?;
        let output_dir = dir.path().join(HOOK_OUTPUTS_DIR);
        let thread_dir = output_dir.join(ThreadId::new().to_string());
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        let old_files = [5_u64, 4, 3, 2]
            .iter()
            .enumerate()
            .map(|(index, age_hours)| {
                let path = thread_dir.join(format!("old-{index}.txt"));
                write_spill(
                    &path,
                    "12345678",
                    now.checked_sub(Duration::from_secs(age_hours * 60 * 60))
                        .context("old time")?,
                )?;
                Ok(path)
            })
            .collect::<Result<Vec<_>>>()?;
        let current = thread_dir.join("current.txt");
        // Older than every candidate and outside active_grace: only explicit
        // protection can preserve this file when either quota is exceeded.
        write_spill(&current, "12345678", now - Duration::from_secs(10 * 60 * 60))?;

        prune_crash_leftovers_at(
            &output_dir,
            Some(&current),
            test_policy(
                Duration::from_secs(30 * 24 * 60 * 60),
                Duration::from_secs(60 * 60),
                max_files,
                max_bytes,
            ),
            now,
        )
        .await?;

        for (index, path) in old_files.iter().enumerate() {
            assert_eq!(path.exists(), index >= first_retained, "{path:?}");
        }
        assert!(current.exists());
    }
    Ok(())
}
