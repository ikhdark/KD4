use super::CompletedExternalAgentSessionImport;
use super::record_completed_session_imports;
use codex_protocol::ThreadId;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use std::sync::Barrier;
use tempfile::TempDir;

#[test]
fn completed_imports_do_not_read_source_files() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    let contents = b"session contents";
    std::fs::write(&source_path, contents).expect("source");
    let source_path = std::fs::canonicalize(&source_path).expect("canonical source");
    std::fs::remove_file(&source_path).expect("remove source");
    let imported_thread_id = ThreadId::new();

    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: format!("{:x}", Sha256::digest(contents)),
            imported_thread_id,
        }],
    )
    .expect("record completed imports");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(ledger.records.len(), 1);
    assert_eq!(ledger.records[0].source_path, source_path);
    assert_eq!(ledger.records[0].imported_thread_id, imported_thread_id);
}

#[test]
fn completed_import_replaces_existing_record_for_the_same_version() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    let contents = b"session contents";
    std::fs::write(&source_path, contents).expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let content_sha256 = format!("{:x}", Sha256::digest(contents));
    let first_thread_id = ThreadId::new();
    let second_thread_id = ThreadId::new();

    for imported_thread_id in [first_thread_id, second_thread_id] {
        record_completed_session_imports(
            &codex_home,
            vec![CompletedExternalAgentSessionImport {
                source_path: source_path.clone(),
                source_content_sha256: content_sha256.clone(),
                imported_thread_id,
            }],
        )
        .expect("record import");
    }

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(ledger.records.len(), 1);
    assert_eq!(ledger.records[0].source_path, source_path);
    assert_eq!(ledger.records[0].imported_thread_id, second_thread_id);
    assert_eq!(ledger.records[0].content_sha256, content_sha256);
}

#[test]
fn ledgers_with_retired_fields_still_load() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).expect("codex home");
    let source_path = root.path().join("session.jsonl");
    let imported_thread_id = ThreadId::new();
    std::fs::write(
        super::import_ledger_path(&codex_home),
        serde_json::json!({
            "records": [{
                "source_path": source_path,
                "content_sha256": "abc",
                "imported_thread_id": imported_thread_id,
                "imported_at": 1,
                "source_modified_at": 2,
            }],
        })
        .to_string(),
    )
    .expect("write ledger");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert!(ledger.contains_fingerprint(&source_path, "abc"));
}

#[test]
fn concurrent_completed_imports_preserve_every_ledger_update() {
    const IMPORT_COUNT: usize = 16;

    let root = TempDir::new().expect("tempdir");
    let codex_home = Arc::new(root.path().join("codex-home"));
    let barrier = Arc::new(Barrier::new(IMPORT_COUNT + 1));
    let mut workers = Vec::new();
    let mut expected = Vec::new();
    let before = super::now_unix_seconds();
    for index in 0..IMPORT_COUNT {
        let source_path = root.path().join(format!("session-{index}.jsonl"));
        let contents = format!("session contents {index}");
        std::fs::write(&source_path, &contents).expect("source");
        let source_path = std::fs::canonicalize(source_path).expect("canonical source");
        let completed_import = CompletedExternalAgentSessionImport {
            source_path,
            source_content_sha256: format!("{:x}", Sha256::digest(contents)),
            imported_thread_id: ThreadId::new(),
        };
        expected.push(completed_import.clone());
        let codex_home = Arc::clone(&codex_home);
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            record_completed_session_imports(codex_home.as_path(), vec![completed_import])
        }));
    }

    barrier.wait();
    for worker in workers {
        worker.join().expect("worker").expect("record import");
    }

    let ledger = super::load_import_ledger(codex_home.as_path()).expect("ledger");
    assert_eq!(ledger.records.len(), IMPORT_COUNT);
    let after = super::now_unix_seconds();
    for expected in expected {
        let actual = ledger
            .records
            .iter()
            .find(|record| record.source_path == expected.source_path)
            .expect("every identity survives");
        assert_eq!(actual.content_sha256, expected.source_content_sha256);
        assert_eq!(actual.imported_thread_id, expected.imported_thread_id);
        assert!((before..=after).contains(&actual.imported_at));
    }
}
