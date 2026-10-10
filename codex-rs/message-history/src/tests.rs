use super::*;
use codex_config::types::History;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

#[tokio::test]
async fn disabled_persistence_does_not_create_or_modify_history() {
    let temp_dir = TempDir::new().expect("temp dir");
    let codex_home = temp_dir.path().join("missing").join("home");
    let config = HistoryConfig {
        codex_home: codex_home.clone(),
        persistence: HistoryPersistence::None,
        max_bytes: Some(1),
    };
    append_entry("do not persist", "session", &config)
        .await
        .expect("disabled history succeeds without storage");
    assert!(!codex_home.exists(), "disabled history must not create directories");

    std::fs::create_dir_all(&codex_home).expect("create fixture directory");
    let path = history_filepath(&config);
    let original = b"existing incomplete history must remain untouched";
    std::fs::write(&path, original).expect("write existing history");
    append_entry("still private", "session", &config)
        .await
        .expect("disabled history leaves existing storage alone");
    assert_eq!(std::fs::read(&path).expect("read existing history"), original);
}

#[tokio::test]
#[cfg(windows)]
async fn retention_publishes_history_without_the_temporary_file_attribute() {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;

    let codex_home = TempDir::new().expect("create temp dir");
    let mut config = HistoryConfig::new(codex_home.path(), &History::default());
    append_entry("first prompt", "session", &config)
        .await
        .expect("create history");
    let (original_id, _) = history_metadata(&config).await;
    config.max_bytes = Some(1);
    append_entry("second prompt", "session", &config)
        .await
        .expect("replace history during retention");

    let (retained_id, count) = history_metadata(&config).await;
    assert_ne!(retained_id, original_id, "retention must publish a new file");
    assert_eq!(count, 1);
    assert_eq!(
        lookup(retained_id, 0, &config).map(|entry| entry.text),
        Some("second prompt".to_string())
    );
    let attributes = std::fs::metadata(codex_home.path().join(HISTORY_FILENAME))
        .expect("history metadata")
        .file_attributes();
    assert_eq!(
        attributes & FILE_ATTRIBUTE_TEMPORARY,
        0,
        "later appends to the published history must not be marked temporary"
    );
}

#[tokio::test]
async fn history_metadata_counts_newlines_across_read_boundaries() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let history_path = temp_dir.path().join(HISTORY_FILENAME);
    let mut contents = vec![b'x'; 3 * HISTORY_READ_BUFFER_SIZE + 1];
    let newline_offsets = [
        0,
        HISTORY_READ_BUFFER_SIZE - 1,
        HISTORY_READ_BUFFER_SIZE,
        2 * HISTORY_READ_BUFFER_SIZE,
        contents.len() - 2,
    ];
    for offset in newline_offsets {
        contents[offset] = b'\n';
    }
    std::fs::write(&history_path, contents).expect("write history file");

    let (_, count) = history_metadata_for_file(&history_path).await;

    assert_eq!(count, newline_offsets.len());
}

#[tokio::test]
async fn lookup_uses_stable_log_id_after_appends() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let history_path = temp_dir.path().join(HISTORY_FILENAME);

    let initial = HistoryEntry {
        session_id: "first-session".to_string(),
        ts: 1,
        text: "first".to_string(),
    };
    let appended = HistoryEntry {
        session_id: "second-session".to_string(),
        ts: 2,
        text: "second".to_string(),
    };

    let mut file = File::create(&history_path).expect("create history file");
    writeln!(
        file,
        "{}",
        serde_json::to_string(&initial).expect("serialize initial entry")
    )
    .expect("write initial entry");

    let (log_id, count) = history_metadata_for_file(&history_path).await;
    assert_eq!(count, 1);
    assert_ne!(log_id, 0, "zero bypasses identity checking in lookup");
    assert_eq!(lookup_history_entry(&history_path, log_id, 0), Some(initial));

    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(&history_path)
        .expect("open history file for append");
    writeln!(
        append,
        "{}",
        serde_json::to_string(&appended).expect("serialize appended entry")
    )
    .expect("append history entry");

    assert_eq!(history_metadata_for_file(&history_path).await, (log_id, 2));
    let fetched = lookup_history_entry(&history_path, log_id, /*offset*/ 1)
        .expect("lookup appended history entry");
    assert_eq!(fetched, appended);
}

#[tokio::test]
async fn append_entry_trims_history_when_beyond_max_bytes() {
    let codex_home = TempDir::new().expect("create temp dir");
    let mut history = History::default();
    let mut config = HistoryConfig::new(codex_home.path(), &history);
    let conversation_id = "conversation-id";

    let entry_one = "a".repeat(200);
    let entry_two = "b".repeat(200);

    let history_path = codex_home.path().join("history.jsonl");

    append_entry(&entry_one, &conversation_id, &config)
        .await
        .expect("write first entry");

    let first_len = std::fs::metadata(&history_path).expect("metadata").len();
    let limit_bytes = first_len + 10;

    history.max_bytes = Some(usize::try_from(limit_bytes).expect("limit should fit into usize"));
    config = HistoryConfig::new(codex_home.path(), &history);

    append_entry(&entry_two, &conversation_id, &config)
        .await
        .expect("write second entry");

    let contents = std::fs::read_to_string(&history_path).expect("read history");

    let entries = contents
        .lines()
        .map(|line| serde_json::from_str::<HistoryEntry>(line).expect("parse entry"))
        .collect::<Vec<HistoryEntry>>();

    assert_eq!(
        entries.len(),
        1,
        "only one entry left because entry_one should be evicted"
    );
    assert_eq!(entries[0].text, entry_two);
    assert!(std::fs::metadata(&history_path).expect("metadata").len() <= limit_bytes);
}

#[tokio::test]
async fn append_entry_trims_history_to_soft_cap() {
    let codex_home = TempDir::new().expect("create temp dir");
    let mut history = History::default();
    let mut config = HistoryConfig::new(codex_home.path(), &history);
    let conversation_id = "conversation-id";

    let short_entry = "a".repeat(200);
    let long_entry = "b".repeat(400);

    let history_path = codex_home.path().join("history.jsonl");

    append_entry(&short_entry, &conversation_id, &config)
        .await
        .expect("write first entry");

    let short_entry_len = std::fs::metadata(&history_path).expect("metadata").len();

    append_entry(&long_entry, &conversation_id, &config)
        .await
        .expect("write second entry");

    let two_entry_len = std::fs::metadata(&history_path).expect("metadata").len();

    let long_entry_len = two_entry_len
        .checked_sub(short_entry_len)
        .expect("second entry length should be larger than first entry length");

    history.max_bytes = Some(
        usize::try_from((2 * long_entry_len) + (short_entry_len / 2))
            .expect("max bytes should fit into usize"),
    );
    config = HistoryConfig::new(codex_home.path(), &history);

    append_entry(&long_entry, &conversation_id, &config)
        .await
        .expect("write third entry");

    let contents = std::fs::read_to_string(&history_path).expect("read history");

    let entries = contents
        .lines()
        .map(|line| serde_json::from_str::<HistoryEntry>(line).expect("parse entry"))
        .collect::<Vec<HistoryEntry>>();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, long_entry);

    let pruned_len = std::fs::metadata(&history_path).expect("metadata").len();
    let max_bytes = config.max_bytes.expect("max bytes should be configured") as u64;

    assert!(pruned_len <= max_bytes);

    let soft_cap_bytes = ((max_bytes as f64) * HISTORY_SOFT_CAP_RATIO)
        .floor()
        .clamp(1.0, max_bytes as f64) as u64;
    let len_without_first = 2 * long_entry_len;

    assert!(
        len_without_first <= max_bytes,
        "dropping only the first entry would satisfy the hard cap"
    );
    assert!(
        len_without_first > soft_cap_bytes,
        "soft cap should require more aggressive trimming than the hard cap"
    );

    assert_eq!(pruned_len, long_entry_len);
    assert!(pruned_len <= soft_cap_bytes.max(long_entry_len));
}

#[tokio::test]
async fn compaction_invalidates_old_offsets_and_preserves_newest_oversized_entry() {
    let home = TempDir::new().unwrap();
    let mut config = HistoryConfig::new(home.path(), &History::default());
    append_entry("first", "session", &config).await.unwrap();
    append_entry("second", "session", &config).await.unwrap();
    let (old_id, _) = history_metadata(&config).await;
    assert_eq!(lookup(old_id, 1, &config).unwrap().text, "second");
    config.max_bytes = Some(1);
    append_entry("newest", "session", &config).await.unwrap();
    let (new_id, count) = history_metadata(&config).await;
    assert_ne!(new_id, old_id);
    assert_eq!(count, 1);
    assert_eq!(lookup(old_id, 0, &config), None);
    assert_eq!(lookup(new_id, 0, &config).unwrap().text, "newest");
}

#[tokio::test]
async fn append_recovers_incomplete_suffix() {
    let home = TempDir::new().unwrap();
    let config = HistoryConfig::new(home.path(), &History::default());
    append_entry("complete", "session", &config).await.unwrap();
    let (id, _) = history_metadata(&config).await;
    let mut file = OpenOptions::new()
        .append(true)
        .open(history_filepath(&config))
        .unwrap();
    file.write_all(&vec![b'x'; HISTORY_READ_BUFFER_SIZE + 10])
        .unwrap();
    drop(file);
    append_entry("recovered", "session", &config).await.unwrap();
    assert_eq!(history_metadata(&config).await, (id, 2));
    assert_eq!(lookup(id, 0, &config).unwrap().text, "complete");
    assert_eq!(lookup(id, 1, &config).unwrap().text, "recovered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_writer_reopens_after_compaction() {
    let home = TempDir::new().unwrap();
    let config = HistoryConfig::new(home.path(), &History::default());
    append_entry("old", "session", &config).await.unwrap();
    append_entry("retained", "session", &config).await.unwrap();
    let path = history_filepath(&config);
    let retained_len = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .len() as u64
        + 1;
    let mut locked = open_locked_history(&path, true).unwrap();
    let writer_config = config.clone();
    let writer =
        tokio::spawn(async move { append_entry("waiting", "session", &writer_config).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    enforce_history_limit(&mut locked, &path, Some(1), retained_len).unwrap();
    drop(locked);
    writer.await.unwrap().unwrap();
    let (id, count) = history_metadata(&config).await;
    assert_eq!(count, 2);
    assert_eq!(lookup(id, 0, &config).unwrap().text, "retained");
    assert_eq!(lookup(id, 1, &config).unwrap().text, "waiting");
}

#[tokio::test]
async fn stale_open_handles_reopen_current_generation_after_compaction() {
    let home = TempDir::new().expect("temp dir");
    let config = HistoryConfig::new(home.path(), &History::default());
    append_entry("old", "session", &config).await.unwrap();
    append_entry("retained", "session", &config).await.unwrap();
    let path = history_filepath(&config);
    let original = std::fs::read_to_string(&path).unwrap();
    let retained = format!("{}\n", original.lines().last().unwrap());
    let mut locked = open_locked_history(&path, true).unwrap();
    let old_id = log_identity(&locked).unwrap();
    // Both handles genuinely refer to the old generation before publication.
    let stale_handles = [false, true].map(|exclusive| {
        let file = OpenOptions::new().read(true).write(exclusive).open(&path).unwrap();
        assert_eq!(log_identity(&file).unwrap(), old_id);
        (exclusive, file)
    });
    enforce_history_limit(&mut locked, &path, Some(1), retained.len() as u64).unwrap();
    drop(locked);
    let (new_id, count) = history_metadata(&config).await;
    assert_ne!(new_id, old_id);
    assert_eq!(count, 1);

    for (exclusive, stale_handle) in stale_handles {
        let mut stale_handle = Some(stale_handle);
        let mut current = open_locked_history_with(&path, exclusive, |options| {
            match stale_handle.take() {
                Some(file) => Ok(file),
                None => options.open(&path),
            }
        })
        .expect("stale reader or writer must reopen the published generation");
        assert!(stale_handle.is_none(), "the old handle must actually be exercised");
        assert_eq!(log_identity(&current).unwrap(), new_id);
        let mut contents = String::new();
        current.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, retained);
    }
}

#[tokio::test]
async fn failed_compaction_publication_preserves_original() {
    let home = TempDir::new().unwrap();
    let config = HistoryConfig::new(home.path(), &History::default());
    append_entry("first", "session", &config).await.unwrap();
    append_entry("newest", "session", &config).await.unwrap();
    let path = history_filepath(&config);
    let original = std::fs::read(&path).unwrap();
    let newest_len = original
        .split_inclusive(|byte| *byte == b'\n')
        .next_back()
        .unwrap()
        .len() as u64;
    let mut locked = open_locked_history(&path, true).unwrap();
    let invalid_destination = home.path().join("directory");
    std::fs::create_dir(&invalid_destination).unwrap();
    assert!(enforce_history_limit(&mut locked, &invalid_destination, Some(1), newest_len).is_err());
    drop(locked);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let (id, count) = history_metadata(&config).await;
    assert_eq!(count, 2);
    assert_eq!(lookup(id, 1, &config).unwrap().text, "newest");
}
