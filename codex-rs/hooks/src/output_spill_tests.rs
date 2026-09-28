use super::*;
use anyhow::Context;
use anyhow::Result;
use std::fs::FileTimes;
use std::path::Path;
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

fn spill_path(output: &str) -> Result<&Path> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("Full hook output saved to: "))
        .map(Path::new)
        .context("spill path")
}

#[tokio::test]
async fn small_hook_output_remains_inline() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller::with_directory(output_dir.clone());
    let output = spiller
        .maybe_spill_text(ThreadId::new(), "short".to_string())
        .await;
    assert_eq!(output, "short");
    assert!(!output_dir.exists());
    Ok(())
}

#[tokio::test]
async fn large_hook_output_spills_to_file() -> Result<()> {
    let dir = tempdir()?;
    let text = "hook output ".repeat(1_000);
    let root = AbsolutePathBuf::from_absolute_path(dir.path())?;
    let spiller = HookOutputSpiller::with_directory(root.clone());
    let thread = ThreadId::new();
    let output = spiller.maybe_spill_text(thread, text.clone()).await;
    assert!(output.contains("[omitted before retained middle]"));
    assert!(output.contains("[omitted after retained middle]"));
    assert!(approx_token_count(&output) <= HOOK_OUTPUT_TOKEN_LIMIT);
    let path = spill_path(&output)?;
    assert!(path.starts_with(root.join(thread.to_string()).join("hooks").as_path()));
    assert_eq!(fs::read_to_string(path).await?, text);
    Ok(())
}

#[tokio::test]
async fn child_outputs_belong_to_the_child_not_the_shared_session() -> Result<()> {
    let dir = tempdir()?;
    let root = AbsolutePathBuf::from_absolute_path(dir.path())?;
    let child = ThreadId::new();
    let shared_session = ThreadId::new();
    let output = HookOutputSpiller::for_thread(root.clone(), child)
        .maybe_spill_text(shared_session, "child evidence ".repeat(2_000))
        .await;
    assert!(spill_path(&output)?.starts_with(root.join(child.to_string()).join("hooks").as_path()));
    assert!(!root.join(shared_session.to_string()).exists());
    Ok(())
}

#[tokio::test]
async fn spilling_in_another_session_and_after_resume_preserves_referenced_outputs() -> Result<()> {
    let dir = tempdir()?;
    let root = AbsolutePathBuf::from_absolute_path(dir.path())?;
    let thread = ThreadId::new();
    let text = "important evidence ".repeat(2_000);
    let preview = HookOutputSpiller::with_directory(root.clone())
        .maybe_spill_text(thread, text.clone())
        .await;
    let referenced = spill_path(&preview)?;
    write_spill(referenced, &text, SystemTime::UNIX_EPOCH)?;
    // Include legacy flat paths: a new writer must not reclaim another chat's
    // old references merely because the former age/count/byte quotas are exceeded.
    for index in 0..513 {
        write_spill(
            &root
                .join(ThreadId::new().to_string())
                .join(format!("{index}.txt")),
            "other evidence",
            SystemTime::UNIX_EPOCH,
        )?;
    }
    let oversized = root.join("old-output.txt");
    std::fs::File::create(&oversized)?.set_len(65 * 1024 * 1024)?;
    let resumed = HookOutputSpiller::with_directory(root);
    let outputs = resumed
        .maybe_spill_texts(ThreadId::new(), vec![text.clone(), text.clone()])
        .await;
    for output in outputs {
        assert_eq!(fs::read_to_string(spill_path(&output)?).await?, text);
        assert!(approx_token_count(&output) <= HOOK_OUTPUT_TOKEN_LIMIT);
    }
    resumed.maybe_spill_text(thread, text.clone()).await;
    assert_eq!(fs::read_to_string(referenced).await?, text);
    assert!(oversized.exists());
    Ok(())
}
