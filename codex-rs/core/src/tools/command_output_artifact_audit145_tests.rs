use super::*;
use super::capture::RawOutputArtifactTask;

thread_local! {
    pub(super) static MATERIALIZED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static REFERENCE_SELECTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[test]
fn oversized_byte_preflight_preserves_the_exact_overflow_with_less_materialization() {
    for byte in [b'x', 0, 0xff] {
        let canonical = CanonicalToolResult::bytes(vec![byte; 4 * 1024 * 1024]);
        let metadata = producer_snapshot_metadata(&canonical, "fixture".into()).unwrap();
        let select = || select_logical_artifact(&metadata, &canonical.bytes,
            ToolOutputSelector::Bytes { start:0, end:canonical.exact_bytes }, 1000, 1000, &[], None);
        REFERENCE_SELECTION.with(|flag| flag.set(true));
        MATERIALIZED_BYTES.with(|count| count.set(0));
        let reference = select();
        let baseline = MATERIALIZED_BYTES.with(std::cell::Cell::get);
        REFERENCE_SELECTION.with(|flag| flag.set(false));
        MATERIALIZED_BYTES.with(|count| count.set(0));
        let bounded = select();
        assert_eq!(serde_json::to_vec(&bounded).unwrap(), serde_json::to_vec(&reference).unwrap());
        assert!(MATERIALIZED_BYTES.with(std::cell::Cell::get) < baseline);
    }
}

#[tokio::test]
async fn independent_snapshots_share_backing_and_partial_install_does_not_truncate_it() {
    let home = tempfile::tempdir().unwrap();
    let canonical = CanonicalToolResult::text("physical source\r\n".repeat(20_000));
    let first = create_source_snapshot_artifact(home.path(), "shared", &canonical, None).await;
    let first_id = first.artifact_id().unwrap();
    let mut ids = BTreeSet::from([first_id.clone()]);
    for _ in 1..20 {
        let artifact = create_source_snapshot_artifact(home.path(), "shared", &canonical, Some(first_id.clone())).await;
        assert!(artifact.complete, "{:?}", artifact.error);
        ids.insert(artifact.artifact_id().unwrap());
    }
    assert_eq!(ids.len(), 20);
    let directory = home.path().join("tool-output/shared");
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        let inodes = ids.iter().map(|id| std::fs::metadata(directory.join(format!("{id}.log"))).unwrap().ino()).collect::<BTreeSet<_>>();
        assert_eq!(inodes.len(), 1, "distinct logical origins must share one immutable data inode");
    }
    let owner = StageOwner::acquire(&directory, ToolOutputArtifactId::new(), true).unwrap();
    let (segments, lease) = stage_shared_source_backing(&owner, &first_id, &canonical).unwrap();
    assert!(segments[0].shared);
    let destination = directory.join(format!("{}.log", owner.id));
    install_staged_logical_segments_blocking(&destination, segments, 7).unwrap();
    drop(lease);
    assert_eq!(std::fs::read(&destination).unwrap(), &canonical.bytes[..7]);
    let original = load_tool_output_snapshot(home.path(), "shared", &first_id).await.unwrap();
    assert_eq!(original.bytes, canonical.bytes);
    assert!(stage_shared_source_backing(&owner, &first_id, &CanonicalToolResult::text("changed")).is_none());
}

#[tokio::test]
async fn dense_raw_lines_do_not_disable_authenticated_byte_recovery() {
    let home = tempfile::tempdir().unwrap();
    for length in [MAX_CANONICAL_LINE_OFFSETS, MAX_CANONICAL_LINE_OFFSETS + 1] {
        let bytes = vec![b'\n'; length];
        let artifact = create_raw_output_artifact(home.path(), "dense", &bytes).await;
        let id = artifact.artifact_id().unwrap().to_string();
        let snapshot = load_tool_output_snapshot(home.path(), "dense", &id).await.unwrap();
        let result = snapshot.select(vec![ToolOutputSelector::Bytes { start:0, end:4 }], 2_000).await.unwrap();
        assert!(result.complete);
        assert_eq!(result.results[0].text.as_deref(), Some("\n\n\n\n"));
        assert_eq!(result.canonical_sha256, hex::encode(Sha256::digest(&bytes)));
        if length > MAX_CANONICAL_LINE_OFFSETS {
            let result = snapshot.select(vec![ToolOutputSelector::Lines { start:1, end:1 }], 2_000).await.unwrap();
            assert!(!result.complete);
            assert_eq!(result.results[0].status, ToolOutputSelectorStatus::Invalid);
        }
    }
}

#[tokio::test]
async fn tiny_binary_is_retained_but_reversible_text_stays_inline() {
    let home = tempfile::tempdir().unwrap();
    for (bytes, binary) in [(b"a\xffz".as_slice(), true), (b"abc\r\n".as_slice(), false)] {
        let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "tiny")));
        let mut task = RawOutputArtifactTask::spawn(Some(Arc::clone(&state)), Default::default()).unwrap();
        for byte in bytes { task.write_chunk(&[*byte]); }
        task.finish().await;
        let artifact = state.lock().await.clone();
        assert_eq!(artifact.artifact_id().is_some(), binary);
        if binary {
            let id = artifact.artifact_id().unwrap().to_string();
            let snapshot = load_tool_output_snapshot(home.path(), "tiny", &id).await.unwrap();
            assert_eq!(snapshot.bytes, bytes);
            assert!(snapshot.metadata.capture_complete);
        }
    }
    // A valid codepoint split across producer calls must not become binary.
    let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "utf8")));
    let mut task = RawOutputArtifactTask::spawn(Some(Arc::clone(&state)), Default::default()).unwrap();
    task.write_chunk(&[0xc3]); task.write_chunk(&[0xa9]); task.finish().await;
    assert!(state.lock().await.is_pending());
}

#[tokio::test]
async fn gaps_preserve_only_surviving_bytes_and_canonicalization_keeps_capture_truth() {
    let home = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "gap")));
    let mut task = RawOutputArtifactTask::spawn(Some(Arc::clone(&state)), Default::default()).unwrap();
    task.write_chunk(b"a\xff"); task.mark_output_gap(); task.write_chunk(b"\0z"); task.finish().await;
    let id = state.lock().await.artifact_id().unwrap().to_string();
    let snapshot = load_tool_output_snapshot(home.path(), "gap", &id).await.unwrap();
    assert_eq!(snapshot.bytes, b"a\xff\0z");
    assert!(!snapshot.metadata.capture_complete);
    let canonical = snapshot.canonical_result();
    assert!(!canonical.complete);
    let packet = create_tool_history_output_artifact(home.path(), "packet", &canonical).await;
    assert!(packet.complete, "the saved packet itself is exact: {:?}", packet.error);
    let saved = load_tool_output_snapshot(home.path(), "packet", &packet.artifact_id().unwrap()).await.unwrap();
    assert!(!saved.metadata.capture_complete);
    assert_eq!(saved.bytes, snapshot.bytes);
    let recovered = saved.select(vec![ToolOutputSelector::Bytes { start:0, end:4 }], 2_000).await.unwrap();
    assert!(recovered.complete, "selection delivery is separate from producer capture");
    assert!(!recovered.capture_complete);
}

#[tokio::test]
async fn committed_orphan_marker_is_reconciled_but_an_active_writer_is_not() {
    let home = tempfile::tempdir().unwrap();
    let canonical = CanonicalToolResult::text("BEGIN\r\nneedle\r\nEND\r\n");
    let artifact = create_canonical_output_artifact(home.path(), "orphan", &canonical).await;
    let id = artifact.artifact_id().unwrap();
    let path = home.path().join("tool-output/orphan").join(format!("{id}.log"));
    std::fs::write(logical_transaction_path(&path), LOGICAL_ARTIFACT_CREATE_TRANSACTION_BYTES).unwrap();
    let writer = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    writer.try_lock().unwrap();
    assert!(matches!(load_tool_output_snapshot(home.path(), "orphan", &id).await, Err(ReadToolOutputError::StillWriting)));
    assert!(logical_transaction_path(&path).exists());
    drop(writer);
    let snapshot = load_tool_output_snapshot(home.path(), "orphan", &id).await.unwrap();
    assert_eq!(snapshot.bytes, canonical.bytes);
    assert!(!logical_transaction_path(&path).exists());
}

#[tokio::test]
async fn targeted_json_index_is_bounded_and_preserves_ambiguity_and_escaping() {
    let bytes = format!("[{}]", vec!["0";150_000].join(",")).into_bytes();
    let index = selective_json::index(&bytes, &["/0".into()]).unwrap();
    assert_eq!(index.pointers.len(), 2);
    assert_eq!(index.pointers["/0"].range, CanonicalByteRange::new(1,2));
    assert!(selective_json::index(&bytes, &[String::new()]).is_none(), "a full oversized directory fails explicitly");
    let home = tempfile::tempdir().unwrap();
    for (bytes, pointer, ambiguous) in [
        (br#"{"x":1,"\u0078":2}"#.as_slice(), "/x", true),
        (br#"{"a/b":{"~":7}}"#.as_slice(), "/a~1b/~0", false),
    ] {
        let artifact = create_raw_output_artifact(home.path(), "json", bytes).await;
        let snapshot = load_tool_output_snapshot(home.path(), "json", &artifact.artifact_id().unwrap().to_string()).await.unwrap();
        let result = snapshot.select(vec![ToolOutputSelector::JsonPointer { pointer:pointer.into() }], 2_000).await.unwrap();
        assert_eq!(!result.complete, ambiguous);
        if !ambiguous { assert_eq!(result.results[0].value, Some(serde_json::json!(7))); }
    }
}
