use super::CompletedExternalAgentSessionImport;
use super::ImportedExternalAgentSessionLedger;
use super::record_completed_session_imports;
use super::record_current_source_refreshes;
use codex_protocol::ThreadId;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use std::sync::Barrier;
use tempfile::TempDir;

#[test]
fn empty_ledger_does_not_read_source() {
    let root = TempDir::new().expect("tempdir");
    let missing_source = root.path().join("missing-session.jsonl");

    assert!(
        !ImportedExternalAgentSessionLedger::default()
            .contains_current_source(&missing_source)
            .expect("empty ledger cannot contain sources")
    );
}

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
            source_modified_at: None,
            imported_thread_id,
        }],
    )
    .expect("record completed imports");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(ledger.records.len(), 1);
    assert_eq!(ledger.records[0].source_path, source_path);
    assert_eq!(ledger.records[0].imported_thread_id, imported_thread_id);
    assert_eq!(ledger.records[0].source_modified_at, None);
}

#[test]
fn completed_import_refreshes_existing_record_metadata() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    let contents = b"session contents";
    std::fs::write(&source_path, contents).expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let content_sha256 = format!("{:x}", Sha256::digest(contents));
    let first_thread_id = ThreadId::new();
    let second_thread_id = ThreadId::new();

    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: content_sha256.clone(),
            source_modified_at: Some(1),
            imported_thread_id: first_thread_id,
        }],
    )
    .expect("record first import");
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: content_sha256,
            source_modified_at: Some(1),
            imported_thread_id: second_thread_id,
        }],
    )
    .expect("record replacement import");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(ledger.records.len(), 1);
    assert_eq!(ledger.records[0].source_path, source_path);
    assert_eq!(ledger.records[0].imported_thread_id, second_thread_id);
    assert_eq!(ledger.records[0].source_modified_at, Some(1));
}

#[test]
fn concurrent_completed_imports_preserve_every_ledger_update() {
    const IMPORT_COUNT: usize = 16;

    let root = TempDir::new().expect("tempdir");
    let codex_home = Arc::new(root.path().join("codex-home"));
    let barrier = Arc::new(Barrier::new(IMPORT_COUNT + 1));
    let mut workers = Vec::new();
    for index in 0..IMPORT_COUNT {
        let source_path = root.path().join(format!("session-{index}.jsonl"));
        let contents = format!("session contents {index}");
        std::fs::write(&source_path, &contents).expect("source");
        let source_path = std::fs::canonicalize(source_path).expect("canonical source");
        let completed_import = CompletedExternalAgentSessionImport {
            source_path,
            source_content_sha256: format!("{:x}", Sha256::digest(contents)),
            source_modified_at: Some(index as i64),
            imported_thread_id: ThreadId::new(),
        };
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
}

#[test]
fn stale_source_refresh_does_not_reorder_a_newer_completed_import() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, b"version-a").expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");

    let version_a_hash = format!("{:x}", Sha256::digest(b"version-a"));
    let version_b_hash = format!("{:x}", Sha256::digest(b"version-b"));
    let version_c_hash = format!("{:x}", Sha256::digest(b"version-c"));
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: version_a_hash.clone(),
            source_modified_at: Some(1),
            imported_thread_id: ThreadId::new(),
        }],
    )
    .expect("record version a");
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: version_b_hash,
            source_modified_at: Some(2),
            imported_thread_id: ThreadId::new(),
        }],
    )
    .expect("record version b");

    let stale_snapshot = super::load_import_ledger(&codex_home).expect("snapshot");
    let stale_refresh = stale_snapshot
        .current_source_refresh(&source_path)
        .expect("refresh")
        .expect("version a was imported");

    std::fs::write(&source_path, b"version-c").expect("replace source");
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: version_c_hash.clone(),
            source_modified_at: Some(3),
            imported_thread_id: ThreadId::new(),
        }],
    )
    .expect("record version c");
    record_current_source_refreshes(&codex_home, vec![stale_refresh])
        .expect("record stale refresh");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(
        ledger.records.last().map(|record| &record.content_sha256),
        Some(&version_c_hash)
    );
}
