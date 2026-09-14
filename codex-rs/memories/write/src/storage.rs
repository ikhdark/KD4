use codex_state::Stage1Output;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use uuid::Uuid;

use crate::ensure_layout;
use crate::generated_files::write_generated_file;
use crate::raw_memories_file;
use crate::rollout_summaries_dir;

/// Rebuild `raw_memories.md` from DB-backed stage-1 outputs.
pub async fn rebuild_raw_memories_file_from_memories(
    root: &Path,
    memories: &[Stage1Output],
    max_raw_memories_for_consolidation: usize,
) -> std::io::Result<()> {
    ensure_layout(root).await?;
    rebuild_raw_memories_file(root, memories, max_raw_memories_for_consolidation).await
}

/// Syncs canonical rollout summary files from DB-backed stage-1 output rows.
pub async fn sync_rollout_summaries_from_memories(
    root: &Path,
    memories: &[Stage1Output],
    max_raw_memories_for_consolidation: usize,
) -> std::io::Result<()> {
    ensure_layout(root).await?;

    let retained = retained_memories(memories, max_raw_memories_for_consolidation);
    let keep = retained
        .iter()
        .map(rollout_summary_file_stem)
        .collect::<HashSet<_>>();
    prune_rollout_summaries(root, &keep).await?;

    for memory in retained {
        write_rollout_summary_for_thread(root, memory).await?;
    }

    Ok(())
}

async fn rebuild_raw_memories_file(
    root: &Path,
    memories: &[Stage1Output],
    max_raw_memories_for_consolidation: usize,
) -> std::io::Result<()> {
    let retained = retained_memories(memories, max_raw_memories_for_consolidation);
    let mut body = String::from("# Raw Memories\n\n");

    if retained.is_empty() {
        body.push_str("No raw memories yet.\n");
        let path = raw_memories_file(root);
        return write_generated_file(&path, &body).await;
    }

    body.push_str("Merged stage-1 raw memories (stable ascending thread-id order):\n\n");
    for memory in retained {
        writeln!(body, "## Thread `{}`", memory.thread_id).map_err(raw_memories_format_error)?;
        writeln!(
            body,
            "updated_at: {}",
            memory.source_updated_at.to_rfc3339()
        )
        .map_err(raw_memories_format_error)?;
        writeln!(body, "cwd: {}", memory.cwd.display()).map_err(raw_memories_format_error)?;
        writeln!(body, "rollout_path: {}", memory.rollout_path.display())
            .map_err(raw_memories_format_error)?;
        let rollout_summary_file = format!("{}.md", rollout_summary_file_stem(memory));
        writeln!(body, "rollout_summary_file: {rollout_summary_file}")
            .map_err(raw_memories_format_error)?;
        writeln!(body).map_err(raw_memories_format_error)?;
        body.push_str(memory.raw_memory.trim());
        body.push_str("\n\n");
    }

    let path = raw_memories_file(root);
    write_generated_file(&path, &body).await
}

async fn prune_rollout_summaries(root: &Path, keep: &HashSet<String>) -> std::io::Result<()> {
    let dir_path = rollout_summaries_dir(root);
    let mut dir = match tokio::fs::read_dir(&dir_path).await {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(".md") else {
            continue;
        };
        if !keep.contains(stem)
            && let Err(err) = tokio::fs::remove_file(&path).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(std::io::Error::new(
                err.kind(),
                format!(
                    "failed pruning outdated rollout summary {}: {err}",
                    path.display()
                ),
            ));
        }
    }

    Ok(())
}

async fn write_rollout_summary_for_thread(
    root: &Path,
    memory: &Stage1Output,
) -> std::io::Result<()> {
    let file_stem = rollout_summary_file_stem(memory);
    let path = rollout_summaries_dir(root).join(format!("{file_stem}.md"));

    let mut body = String::new();
    writeln!(body, "thread_id: {}", memory.thread_id).map_err(rollout_summary_format_error)?;
    writeln!(
        body,
        "updated_at: {}",
        memory.source_updated_at.to_rfc3339()
    )
    .map_err(rollout_summary_format_error)?;
    writeln!(body, "rollout_path: {}", memory.rollout_path.display())
        .map_err(rollout_summary_format_error)?;
    writeln!(body, "cwd: {}", memory.cwd.display()).map_err(rollout_summary_format_error)?;
    if let Some(git_branch) = memory.git_branch.as_deref() {
        writeln!(body, "git_branch: {git_branch}").map_err(rollout_summary_format_error)?;
    }
    writeln!(body).map_err(rollout_summary_format_error)?;
    body.push_str(&memory.rollout_summary);
    body.push('\n');

    write_generated_file(&path, &body).await
}

fn retained_memories(
    memories: &[Stage1Output],
    max_raw_memories_for_consolidation: usize,
) -> &[Stage1Output] {
    &memories[..memories.len().min(max_raw_memories_for_consolidation)]
}

fn raw_memories_format_error(err: std::fmt::Error) -> std::io::Error {
    std::io::Error::other(format!("format raw memories: {err}"))
}

fn rollout_summary_format_error(err: std::fmt::Error) -> std::io::Error {
    std::io::Error::other(format!("format rollout summary: {err}"))
}

pub fn rollout_summary_file_stem(memory: &Stage1Output) -> String {
    rollout_summary_file_stem_from_parts(
        memory.thread_id,
        memory.source_updated_at,
        memory.rollout_slug.as_deref(),
    )
}

fn rollout_summary_file_stem_from_parts(
    thread_id: codex_protocol::ThreadId,
    source_updated_at: chrono::DateTime<chrono::Utc>,
    rollout_slug: Option<&str>,
) -> String {
    const ROLLOUT_SLUG_MAX_LEN: usize = 60;
    let thread_id = thread_id.to_string();
    let timestamp = Uuid::parse_str(&thread_id)
        .ok()
        .and_then(|uuid| uuid.get_timestamp())
        .and_then(|timestamp| {
            let (seconds, nanos) = timestamp.to_unix();
            chrono::DateTime::<chrono::Utc>::from_timestamp(i64::try_from(seconds).ok()?, nanos)
        })
        .unwrap_or(source_updated_at);
    let file_prefix = format!("{}-{thread_id}", timestamp.format("%Y-%m-%dT%H-%M-%S"));

    let Some(raw_slug) = rollout_slug else {
        return file_prefix;
    };

    let mut slug = String::with_capacity(ROLLOUT_SLUG_MAX_LEN);
    for ch in raw_slug.chars() {
        if slug.len() >= ROLLOUT_SLUG_MAX_LEN {
            break;
        }

        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else {
            slug.push('_');
        }
    }

    while slug.ends_with('_') {
        slug.pop();
    }

    if slug.is_empty() {
        file_prefix
    } else {
        format!("{file_prefix}-{slug}")
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
