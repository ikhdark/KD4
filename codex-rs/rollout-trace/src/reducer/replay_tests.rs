use serde_json::json;
use tempfile::TempDir;

use crate::RawTraceEventPayload;
use crate::RolloutStatus;
use crate::TraceWriter;
use crate::replay_bundle;

#[test]
fn replay_rejects_invalid_event_envelopes_without_rewriting_evidence() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let writer = TraceWriter::create(temp.path(), "trace".into(), "rollout".into(), "root".into())?;
    let started = writer.append(RawTraceEventPayload::RolloutStarted {
        trace_id: "trace".into(),
        root_thread_id: "root".into(),
    })?;
    let ended = writer.append(RawTraceEventPayload::RolloutEnded {
        status: RolloutStatus::Completed,
    })?;
    let started = serde_json::to_value(started)?;
    let ended = serde_json::to_value(ended)?;
    let log = temp.path().join("trace.jsonl");
    assert_eq!(replay_bundle(temp.path())?.status, RolloutStatus::Completed);

    for (field, value, expected) in [
        ("seq", json!(1), "sequence 1, expected 2"),
        ("seq", json!(3), "sequence 3, expected 2"),
        ("rollout_id", json!("other"), "belongs to rollout other"),
        ("schema_version", json!(2), "unsupported trace event schema"),
    ] {
        let mut invalid = ended.clone();
        invalid[field] = value;
        let bytes = format!("{started}\n{invalid}\n").into_bytes();
        std::fs::write(&log, &bytes)?;
        let error = replay_bundle(temp.path()).expect_err("invalid envelope must fail");
        assert!(error.to_string().contains(expected), "{error:#}");
        assert_eq!(std::fs::read(&log)?, bytes);
    }

    // An omitted first record or reordered log must not look like a complete capture.
    std::fs::write(&log, format!("{ended}\n{started}\n"))?;
    let error = replay_bundle(temp.path()).expect_err("out-of-order log must fail");
    assert!(error.to_string().contains("sequence 2, expected 1"));

    // Blank physical lines do not consume writer sequence numbers.
    std::fs::write(&log, format!("\n{started}\n\n{ended}"))?;
    assert_eq!(replay_bundle(temp.path())?.status, RolloutStatus::Completed);
    Ok(())
}

#[test]
fn replay_rejects_manifest_schema_and_conflicting_start_identity() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let writer = TraceWriter::create(temp.path(), "trace".into(), "rollout".into(), "root".into())?;
    let event = writer.append(RawTraceEventPayload::RolloutStarted {
        trace_id: "trace".into(),
        root_thread_id: "root".into(),
    })?;
    let log = temp.path().join("trace.jsonl");
    for field in ["trace_id", "root_thread_id"] {
        let mut invalid = event.clone();
        invalid.payload = if field == "trace_id" {
            RawTraceEventPayload::RolloutStarted {
                trace_id: "other".into(),
                root_thread_id: "root".into(),
            }
        } else {
            RawTraceEventPayload::RolloutStarted {
                trace_id: "trace".into(),
                root_thread_id: "other".into(),
            }
        };
        std::fs::write(&log, format!("{}\n", serde_json::to_string(&invalid)?))?;
        let error = replay_bundle(temp.path()).expect_err("identity mismatch must fail");
        assert!(error.to_string().contains("does not match trace manifest"));
    }
    std::fs::write(&log, format!("{}\n", serde_json::to_string(&event)?))?;
    let manifest_path = temp.path().join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    manifest["schema_version"] = json!(2);
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
    let error = replay_bundle(temp.path()).expect_err("unknown schema must fail");
    assert!(
        error
            .to_string()
            .contains("unsupported trace manifest schema")
    );
    Ok(())
}
