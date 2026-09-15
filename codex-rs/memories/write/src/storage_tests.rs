use super::rollout_summary_file_stem;
use crate::ensure_layout;
use crate::raw_memories_file;
use crate::rebuild_raw_memories_file_from_memories;
use crate::rollout_summaries_dir;
use crate::sync_rollout_summaries_from_memories;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_state::Stage1Output;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use tempfile::tempdir;

const FIXED_PREFIX: &str = "2025-02-11T15-35-19-0194f5a6-89ab-7cde-8123-456789abcdef";

fn stage1_output_with_slug(thread_id: ThreadId, rollout_slug: Option<&str>) -> Stage1Output {
    Stage1Output {
        thread_id,
        source_updated_at: Utc.timestamp_opt(123, 0).single().expect("timestamp"),
        raw_memory: "raw memory".to_string(),
        rollout_summary: "summary".to_string(),
        rollout_slug: rollout_slug.map(ToString::to_string),
        rollout_path: PathBuf::from("/tmp/rollout.jsonl"),
        cwd: PathBuf::from("/tmp/workspace"),
        git_branch: None,
        generated_at: Utc.timestamp_opt(124, 0).single().expect("timestamp"),
    }
}

fn fixed_thread_id() -> ThreadId {
    ThreadId::try_from("0194f5a6-89ab-7cde-8123-456789abcdef").expect("valid thread id")
}

#[test]
fn rollout_summary_file_stem_uses_uuid_timestamp_and_full_identity_when_slug_missing() {
    let thread_id = fixed_thread_id();
    let memory = stage1_output_with_slug(thread_id, /*rollout_slug*/ None);

    assert_eq!(rollout_summary_file_stem(&memory), FIXED_PREFIX);
}

#[test]
fn rollout_summary_file_stem_sanitizes_and_truncates_slug() {
    let thread_id = fixed_thread_id();
    let memory = stage1_output_with_slug(
        thread_id,
        Some("Unsafe Slug/With Spaces & Symbols + EXTRA_LONG_12345_67890_ABCDE_fghij_klmno"),
    );

    let stem = rollout_summary_file_stem(&memory);
    let slug = stem
        .strip_prefix(&format!("{FIXED_PREFIX}-"))
        .expect("slug suffix should be present");
    assert_eq!(slug.len(), 60);
    assert_eq!(
        slug,
        "unsafe_slug_with_spaces___symbols___extra_long_12345_67890_a"
    );
}

#[test]
fn rollout_summary_file_stem_uses_uuid_timestamp_and_full_identity_when_slug_is_empty() {
    let thread_id = fixed_thread_id();
    let memory = stage1_output_with_slug(thread_id, Some(""));

    assert_eq!(rollout_summary_file_stem(&memory), FIXED_PREFIX);
}

#[tokio::test]
async fn sync_rollout_summaries_and_raw_memories_file_keeps_latest_memories_only() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().join("memory");
    ensure_layout(&root).await.expect("ensure layout");

    let keep_id = ThreadId::default().to_string();
    let drop_id = ThreadId::default().to_string();
    let keep_path = rollout_summaries_dir(&root).join(format!("{keep_id}.md"));
    let drop_path = rollout_summaries_dir(&root).join(format!("{drop_id}.md"));
    tokio::fs::write(&keep_path, "keep")
        .await
        .expect("write keep");
    tokio::fs::write(&drop_path, "drop")
        .await
        .expect("write drop");

    let mut memories = vec![Stage1Output {
        thread_id: ThreadId::try_from(keep_id.clone()).expect("thread id"),
        source_updated_at: Utc.timestamp_opt(100, 0).single().expect("timestamp"),
        raw_memory: "raw memory".to_string(),
        rollout_summary: "short summary".to_string(),
        rollout_slug: None,
        rollout_path: PathBuf::from("/tmp/rollout-100.jsonl"),
        cwd: PathBuf::from("/tmp/workspace"),
        git_branch: None,
        generated_at: Utc.timestamp_opt(101, 0).single().expect("timestamp"),
    }];

    memories.push(stage1_output_with_slug(
        ThreadId::try_from(drop_id.clone()).unwrap(),
        None,
    ));

    sync_rollout_summaries_from_memories(&root, &memories, 1)
        .await
        .expect("sync rollout summaries");
    rebuild_raw_memories_file_from_memories(&root, &memories, 1)
        .await
        .expect("rebuild raw memories");

    assert!(
        !tokio::fs::try_exists(&keep_path)
            .await
            .expect("check stale keep path"),
        "sync should prune stale filename that used thread id only"
    );
    assert!(
        !tokio::fs::try_exists(&drop_path)
            .await
            .expect("check stale drop path"),
        "sync should prune stale filename for dropped thread"
    );

    let mut dir = tokio::fs::read_dir(rollout_summaries_dir(&root))
        .await
        .expect("open rollout summaries dir");
    let mut files = Vec::new();
    while let Some(entry) = dir.next_entry().await.expect("read dir entry") {
        files.push(entry.file_name().to_string_lossy().to_string());
    }
    files.sort_unstable();
    assert_eq!(files.len(), 1);
    let canonical_rollout_summary_file = &files[0];

    let raw_memories = tokio::fs::read_to_string(raw_memories_file(&root))
        .await
        .expect("read raw memories");
    assert!(raw_memories.contains("raw memory"));
    assert!(raw_memories.contains(&keep_id));
    assert!(!raw_memories.contains(&drop_id));
    assert!(raw_memories.contains("cwd: /tmp/workspace"));
    assert!(raw_memories.contains("rollout_path: /tmp/rollout-100.jsonl"));
    assert!(raw_memories.contains(&format!(
        "rollout_summary_file: {canonical_rollout_summary_file}"
    )));
}

#[tokio::test]
async fn sync_preserves_summaries_with_previously_colliding_thread_ids() {
    let dir = tempdir().unwrap();
    let a = stage1_output_with_slug(fixed_thread_id(), None);
    let mut b = stage1_output_with_slug(
        ThreadId::try_from("0194f5a6-89ab-7cdf-8123-456789abcdef").unwrap(),
        None,
    );
    b.rollout_summary = "second summary".to_string();
    let memories = [a, b];
    sync_rollout_summaries_from_memories(dir.path(), &memories, 2)
        .await
        .unwrap();
    rebuild_raw_memories_file_from_memories(dir.path(), &memories, 2)
        .await
        .unwrap();
    let raw = tokio::fs::read_to_string(raw_memories_file(dir.path()))
        .await
        .unwrap();
    for memory in &memories {
        let name = format!("{}.md", rollout_summary_file_stem(memory));
        let summary = tokio::fs::read_to_string(rollout_summaries_dir(dir.path()).join(&name))
            .await
            .unwrap();
        assert!(summary.contains(&format!("thread_id: {}", memory.thread_id)));
        assert!(summary.contains(&memory.rollout_summary));
        assert!(raw.contains(&format!("rollout_summary_file: {name}")));
    }
}

#[tokio::test]
async fn sync_rejects_failed_stale_summary_removal() {
    let dir = tempdir().unwrap();
    let stale = rollout_summaries_dir(dir.path()).join("stale.md");
    tokio::fs::create_dir_all(&stale).await.unwrap();
    let err = sync_rollout_summaries_from_memories(dir.path(), &[], 0)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("stale.md"));
    assert!(stale.is_dir());
}
