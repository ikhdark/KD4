use super::*;

fn origin(state: &mut ToolHistoryState, id: &str, bytes: u64, sha: &str) {
    assert!(ToolHistoryMutation::RegisterArtifactOrigin {
        artifact_id: id.into(), call_id: "read".into(), bytes, sha256: sha.into(),
    }.apply(state));
}

fn recover(state: &mut ToolHistoryState, id: &str, start: u64, end: u64) {
    ToolHistoryMutation::RecordArtifactRecovery {
        artifact_id: id.into(), recovery_call_id: "recovery".into(),
        selectors: vec![serde_json::json!({"kind":"bytes", "start":start, "end":end})],
    }.apply(state);
}

#[test]
fn envelope_recovery_never_completes_source_coverage_and_legacy_is_unknown() {
    let text = "source bytes";
    let hash = sha256(text.as_bytes());
    let output = serde_json::json!({
        "path":"audit145.txt", "environment_id":"local", "canonical_uri":"file:///audit145.txt",
        "source_sha256":hash, "canonical_bytes":text.len(), "artifact_id":"source",
        "results":[{"selector":{"kind":"bytes", "start":0,"end":6},
            "status":"ok", "complete":true, "canonical_range":{"start":0,"end":6},
            "text":"source", "exact_bytes":6}]
    }).to_string();
    let mut state = ToolHistoryState::default();
    origin(&mut state, "source", text.len() as u64, &hash);
    origin(&mut state, "envelope", output.len() as u64, &sha256(output.as_bytes()));
    state.register(ToolHistoryCandidate {
        call_id:"read".into(), tool_identity:"read_file".into(), semantic_class:"source".into(),
        successful:true, source_dependencies:BTreeSet::new(), source_dependencies_current:true,
        artifact_id:"envelope".into(), artifact_bytes:output.len() as u64,
        artifact_sha256:sha256(output.as_bytes()), original_output_sha256:sha256(output.as_bytes()),
        original_tokens:1000, preserved_non_text_tokens:None, bounded_model_output:output,
        complete:true, projection_eligible:true, proof_identity:None, supersession_identity:None,
        consumed_by_generation:None, derived:Default::default(),
    });
    let coverage = |state: &ToolHistoryState| state.read_status_page(
        &[PathBuf::from("audit145.txt")], Some("local"), &[], &ReadStatusQuery::default(),
    )["paths"][0]["snapshots"][0].clone();
    assert_eq!(coverage(&state)["obtained_bytes"], 6);
    recover(&mut state, "envelope", 0, text.len() as u64);
    assert_eq!(coverage(&state)["obtained_bytes"], 6);
    assert!(state.recovered_artifact_selectors("source").is_none());
    // Historical unqualified checkpoints cannot be promoted to file bytes.
    state.recovered_ranges.insert("read".into(), vec![serde_json::json!({"kind":"bytes","start":0,"end":text.len()})]);
    let mut restored: ToolHistoryState = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    restored.rebuild_artifact_index();
    assert_eq!(coverage(&restored)["obtained_bytes"], 6);
    assert_eq!(coverage(&restored)["coverage_history_complete"], false);
    recover(&mut restored, "source", 6, text.len() as u64);
    assert_eq!(coverage(&restored)["coverage"], "full");
    assert_eq!(coverage(&restored)["obtained_bytes"], text.len());
    restored.internal_artifact_origins.get_mut("source").unwrap().2 = sha256(b"changed source");
    assert!(restored.recovered_artifact_selectors("source").is_none());
}

#[tokio::test]
async fn qualified_recovery_survives_fork_without_pooling_origins() {
    let home = tempfile::tempdir().unwrap();
    let canonical = codex_tools::CanonicalToolResult::text("immutable source");
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(home.path(), "parent", &canonical).await;
    let id = artifact.artifact_id().unwrap();
    let mut state = ToolHistoryState::default();
    origin(&mut state, &id, canonical.exact_bytes, &canonical.sha256);
    recover(&mut state, &id, 0, canonical.exact_bytes);
    let (fork, dropped) = remint_tool_history_state_for_fork(home.path(), "parent", "child", state).await;
    assert_eq!(dropped, 0);
    let (new_id, _) = fork.artifact_references().into_iter().next().unwrap();
    assert_eq!(fork.recovered_artifact_selectors(&new_id).unwrap().len(), 1);
    let recovered = crate::tools::command_output_artifact::load_tool_output_snapshot(home.path(), "child", &new_id).await.unwrap()
        .select(vec![crate::tools::command_output_artifact::ToolOutputSelector::Bytes { start:0, end:canonical.exact_bytes }], 10_000).await.unwrap();
    assert_eq!(recovered.results[0].text.as_deref(), Some("immutable source"));
}

fn record(sequence: u64) -> String {
    let mutation = ToolHistoryMutation::MarkArtifactRecovered { artifact_id:"missing".into() };
    let checksum = tool_history_journal_checksum("writer", sequence, &mutation).unwrap();
    format!("{}\n", serde_json::json!({"version":JOURNAL_VERSION, "writer_id":"writer", "sequence":sequence,
        "mutation":mutation, "checksum_sha256":checksum}))
}

#[test]
fn journal_requires_the_first_uncheckpointed_sequence_and_preserves_torn_tail() {
    for (checkpoint, first, ok) in [(0,1,true),(0,2,false),(1,2,true),(10,11,true),(10,12,false)] {
        let mut sequences = BTreeMap::from([("writer".into(), checkpoint)]);
        let result = replay_tool_history_journal_bytes(Path::new("journal"), record(first).as_bytes(),
            &mut ToolHistoryState::default(), &mut sequences, true);
        assert_eq!(result.is_ok(), ok, "checkpoint {checkpoint}, first {first}");
    }
    let bytes = format!("{}{}{{\"version\":", record(1), record(2));
    let mut sequences = BTreeMap::new();
    replay_tool_history_journal_bytes(Path::new("journal"), bytes.as_bytes(), &mut ToolHistoryState::default(), &mut sequences, true).unwrap();
    assert_eq!(sequences["writer"], 2);
    let mut sequences = BTreeMap::from([("writer".into(), 2)]);
    replay_tool_history_journal_bytes(Path::new("journal"), format!("{}{}",record(2),record(3)).as_bytes(), &mut ToolHistoryState::default(), &mut sequences, true).unwrap();
    assert_eq!(sequences["writer"], 3);
}

#[test]
fn version_decode_preserves_future_corrupt_duplicate_and_trailing_classes() {
    for version in [2,300] {
        let bytes = format!("{{\"version\":{version},\"future_schema\":[]}}");
        assert!(matches!(decode_tool_history_version::<ToolHistoryLedgerFile>(bytes.as_bytes(), LEDGER_VERSION),
            Err(ToolHistoryJournalLoadError::UnsupportedVersion(found)) if found == version));
    }
    for bytes in [b"{\"version\":1,\"state\":[]}".as_slice(),
        b"{\"version\":1,\"state\":{},\"version\":1}", b"{\"version\":1,\"state\":{}}garbage"] {
        assert!(matches!(decode_tool_history_version::<ToolHistoryLedgerFile>(bytes, LEDGER_VERSION), Err(ToolHistoryJournalLoadError::Corrupt(_))));
    }
    let bytes = serde_json::to_vec(&ToolHistoryLedgerFile { version:LEDGER_VERSION, journal_sequences:BTreeMap::new(), state:ToolHistoryState::default() }).unwrap();
    let decoded = decode_tool_history_version::<ToolHistoryLedgerFile>(&bytes, LEDGER_VERSION).unwrap();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
}
