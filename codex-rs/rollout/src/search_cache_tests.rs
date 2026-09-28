use super::*;

#[tokio::test]
async fn cached_search_reuses_reads_and_tracks_physical_changes() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    let archived = home.path().join("archived_sessions");
    std::fs::create_dir_all(&sessions)?;
    std::fs::create_dir_all(&archived)?;
    let first =
        sessions.join("rollout-2026-09-27T12-00-00-00000000-0000-0000-0000-000000000001.jsonl.zst");
    let second =
        sessions.join("rollout-2026-09-27T12-00-00-00000000-0000-0000-0000-000000000002.jsonl.zst");
    let write = |path: &Path, message: &str| -> anyhow::Result<()> {
        let line = serde_json::json!({"timestamp":"2026-09-27T12:00:00Z","type":"event_msg","payload":{"type":"agent_message","message":message}}).to_string() + "\n";
        std::fs::write(path, zstd::stream::encode_all(line.as_bytes(), 3)?)?;
        Ok(())
    };
    write(&first, "needle one")?;
    write(&second, "no match")?;
    let rg = home.path().join("missing-rg");
    let mut cache = RolloutSearchCache::default();
    let initial = cache.search(&rg, home.path(), false, "needle").await?;
    assert_eq!(initial.len(), 1);
    assert_eq!(
        initial[&compression::plain_rollout_path(&first)].as_deref(),
        Some("needle one")
    );
    assert_eq!(cache.decoded_files, 2);
    assert_eq!(
        cache.search(&rg, home.path(), false, "needle").await?,
        initial
    );
    assert_eq!(
        cache.decoded_files, 2,
        "unchanged matches and misses must both be reused"
    );
    write(&second, "needle newly appended content")?;
    assert_eq!(
        cache.search(&rg, home.path(), false, "needle").await?.len(),
        2
    );
    assert_eq!(cache.decoded_files, 3);
    std::fs::rename(&first, archived.join(first.file_name().unwrap()))?;
    assert_eq!(
        cache.search(&rg, home.path(), false, "needle").await?.len(),
        1
    );
    assert_eq!(cache.files.len(), 1);
    assert_eq!(
        cache.search(&rg, home.path(), true, "needle").await?.len(),
        1
    );
    assert!(
        cache
            .search(&rg, home.path(), true, "absent")
            .await?
            .is_empty()
    );
    std::fs::remove_file(&second)?;
    assert!(
        cache
            .search(&rg, home.path(), false, "needle")
            .await?
            .is_empty()
    );
    assert!(cache.files.is_empty());
    Ok(())
}
