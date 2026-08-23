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

#[tokio::test]
async fn small_hook_output_remains_inline() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let thread_id = ThreadId::new();
    let spiller = HookOutputSpiller {
        output_dir: output_dir.clone(),
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
    let spiller = HookOutputSpiller { output_dir };

    let output = spiller
        .maybe_spill_text(ThreadId::new(), text.clone())
        .await;

    assert!(output.contains("tokens truncated"));
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("Full hook output saved to: "))
        .context("spill path")?;
    assert_eq!(fs::read_to_string(path).await?, text);
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
    let dir = tempdir()?;
    let output_dir = dir.path().join(HOOK_OUTPUTS_DIR);
    let thread_dir = output_dir.join(ThreadId::new().to_string());
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
    let ages = [5_u64, 4, 3, 2];
    let old_files = ages
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
    write_spill(&current, "12345678", now)?;

    prune_crash_leftovers_at(
        &output_dir,
        Some(&current),
        test_policy(
            Duration::from_secs(30 * 24 * 60 * 60),
            Duration::from_secs(60 * 60),
            3,
            16,
        ),
        now,
    )
    .await?;

    assert!(!old_files[0].exists());
    assert!(!old_files[1].exists());
    assert!(!old_files[2].exists());
    assert!(old_files[3].exists());
    assert!(current.exists());
    Ok(())
}
