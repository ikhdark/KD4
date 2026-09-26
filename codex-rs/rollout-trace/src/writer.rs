//! Hot-path trace bundle writer.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;

use crate::bundle::MANIFEST_FILE_NAME;
use crate::bundle::PAYLOADS_DIR_NAME;
use crate::bundle::RAW_EVENT_LOG_FILE_NAME;
use crate::bundle::TraceBundleManifest;
use crate::model::AgentThreadId;
use crate::payload::RawPayloadKind;
use crate::payload::RawPayloadRef;
use crate::raw_event::RAW_TRACE_EVENT_SCHEMA_VERSION;
use crate::raw_event::RawTraceEvent;
use crate::raw_event::RawTraceEventContext;
use crate::raw_event::RawTraceEventPayload;

/// Local trace bundle writer.
///
/// The writer appends raw events and writes payload files. It does not keep a
/// reduced `RolloutTrace` in memory; replay is owned by the reducer.
#[derive(Debug)]
pub struct TraceWriter {
    inner: Mutex<TraceWriterInner>,
}

#[derive(Debug)]
struct TraceWriterInner {
    manifest: TraceBundleManifest,
    payloads_dir: PathBuf,
    event_log: BufWriter<File>,
    next_seq: u64,
    next_payload_ordinal: u64,
    append_failed: bool,
}

impl TraceWriter {
    /// Creates a trace bundle directory and writes its manifest.
    pub fn create(
        bundle_dir: impl AsRef<Path>,
        trace_id: String,
        rollout_id: String,
        root_thread_id: AgentThreadId,
    ) -> Result<Self> {
        let bundle_dir = bundle_dir.as_ref().to_path_buf();
        let payloads_dir = bundle_dir.join(PAYLOADS_DIR_NAME);
        std::fs::create_dir_all(&bundle_dir)?;
        anyhow::ensure!(
            !bundle_dir.join(MANIFEST_FILE_NAME).try_exists()? && !payloads_dir.try_exists()?,
            "trace bundle already contains a manifest or payload directory"
        );
        // Claim the log before writing anything else. Concurrent creators cannot
        // reset identities or replace evidence belonging to the winner.
        let event_log_path = bundle_dir.join(RAW_EVENT_LOG_FILE_NAME);
        let event_log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&event_log_path)
            .with_context(|| format!("create trace event log {}", event_log_path.display()))?;
        std::fs::create_dir(&payloads_dir)
            .with_context(|| format!("create trace payload dir {}", payloads_dir.display()))?;

        let started_at_unix_ms = unix_time_ms();
        let manifest =
            TraceBundleManifest::new(trace_id, rollout_id, root_thread_id, started_at_unix_ms);
        write_json_file(&bundle_dir.join(MANIFEST_FILE_NAME), &manifest)?;

        Ok(Self {
            inner: Mutex::new(TraceWriterInner {
                manifest,
                payloads_dir,
                event_log: BufWriter::new(event_log),
                next_seq: 1,
                next_payload_ordinal: 1,
                append_failed: false,
            }),
        })
    }

    /// Writes a JSON payload file and returns its reduced-state reference.
    pub fn write_json_payload(
        &self,
        kind: RawPayloadKind,
        value: &impl Serialize,
    ) -> Result<RawPayloadRef> {
        let mut inner = self.lock_inner();
        let ordinal = inner.next_payload_ordinal;
        inner.next_payload_ordinal += 1;
        let raw_payload_id = format!("raw_payload:{ordinal}");
        let relative_path = format!("{PAYLOADS_DIR_NAME}/{ordinal}.json");
        let absolute_path = inner.payloads_dir.join(format!("{ordinal}.json"));
        drop(inner);
        // Payload files are created before the event that references them. A
        // replay interrupted after an event is appended should never point at a
        // payload file that the writer planned but had not written yet.
        write_json_file(&absolute_path, value)?;
        Ok(RawPayloadRef {
            raw_payload_id,
            kind,
            path: relative_path,
        })
    }

    /// Best-effort payload write used by hot-path trace producers.
    pub(crate) fn write_json_payload_best_effort(
        &self,
        kind: RawPayloadKind,
        value: &impl Serialize,
    ) -> Option<RawPayloadRef> {
        match self.write_json_payload(kind, value) {
            Ok(payload_ref) => Some(payload_ref),
            Err(err) => {
                tracing::warn!("failed to write rollout trace payload: {err:#}");
                None
            }
        }
    }

    /// Appends one raw event with no extra envelope context.
    pub fn append(&self, payload: RawTraceEventPayload) -> Result<RawTraceEvent> {
        self.append_with_context(RawTraceEventContext::default(), payload)
    }

    /// Best-effort event append used by hot-path trace producers.
    pub(crate) fn append_best_effort(&self, payload: RawTraceEventPayload) {
        if let Err(err) = self.append(payload) {
            tracing::warn!("failed to append rollout trace event: {err:#}");
        }
    }

    /// Appends one raw event with explicit thread/turn context.
    pub fn append_with_context(
        &self,
        context: RawTraceEventContext,
        payload: RawTraceEventPayload,
    ) -> Result<RawTraceEvent> {
        let mut inner = self.lock_inner();
        anyhow::ensure!(
            !inner.append_failed,
            "trace event log is no longer writable"
        );
        let event = RawTraceEvent {
            schema_version: RAW_TRACE_EVENT_SCHEMA_VERSION,
            seq: inner.next_seq,
            wall_time_unix_ms: unix_time_ms(),
            rollout_id: inner.manifest.rollout_id.clone(),
            thread_id: context.thread_id,
            codex_turn_id: context.codex_turn_id,
            payload,
        };
        // A serialization panic or partial write must prevent another record
        // from being appended to an uncertain JSONL boundary.
        inner.append_failed = true;
        serde_json::to_writer(&mut inner.event_log, &event)?;
        inner.event_log.write_all(b"\n")?;
        inner.event_log.flush()?;
        inner.append_failed = false;
        inner.next_seq += 1;
        Ok(event)
    }

    /// Best-effort contextual event append used by hot-path trace producers.
    pub(crate) fn append_with_context_best_effort(
        &self,
        context: RawTraceEventContext,
        payload: RawTraceEventPayload,
    ) {
        if let Err(err) = self.append_with_context(context, payload) {
            tracing::warn!("failed to append rollout trace event: {err:#}");
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, TraceWriterInner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            let mut inner = poisoned.into_inner();
            inner.append_failed = true;
            inner
        })
    }
}

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    let mut output = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut output, value)
        .with_context(|| format!("write JSON {}", path.display()))?;
    output
        .flush()
        .with_context(|| format!("flush JSON {}", path.display()))
}

pub(crate) fn unix_time_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::model::ExecutionStatus;
    use crate::model::RolloutStatus;
    use crate::payload::RawPayloadKind;
    use crate::raw_event::RawTraceEventPayload;
    use crate::replay_bundle;
    use crate::writer::TraceWriter;

    #[test]
    fn create_rejects_existing_bundle_evidence_without_overwriting() -> anyhow::Result<()> {
        for name in ["manifest.json", "trace.jsonl", "payloads"] {
            let temp = TempDir::new()?;
            let evidence = temp.path().join(name);
            if name == "payloads" {
                std::fs::create_dir(&evidence)?;
                std::fs::write(evidence.join("1.json"), b"original")?;
            } else {
                std::fs::write(&evidence, b"original")?;
            }
            assert!(
                TraceWriter::create(temp.path(), "new".into(), "new".into(), "root".into())
                    .is_err()
            );
            assert_eq!(
                std::fs::read(if name == "payloads" {
                    evidence.join("1.json")
                } else {
                    evidence
                })?,
                b"original"
            );
        }
        Ok(())
    }

    #[test]
    fn append_failure_disables_subsequent_writes() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let writer =
            TraceWriter::create(temp.path(), "trace".into(), "rollout".into(), "root".into())?;
        let event = RawTraceEventPayload::RolloutStarted {
            trace_id: "trace".into(),
            root_thread_id: "root".into(),
        };
        writer.append(event.clone())?;
        let log = temp.path().join("trace.jsonl");
        let before = std::fs::read(&log)?;
        // A read-only handle forces a real flush error after serialization.
        writer.lock_inner().event_log = std::io::BufWriter::new(std::fs::File::open(&log)?);
        assert!(writer.append(event.clone()).is_err());
        writer.lock_inner().event_log =
            std::io::BufWriter::new(std::fs::OpenOptions::new().append(true).open(&log)?);
        assert!(writer.append(event).is_err());
        assert_eq!(std::fs::read(&log)?, before);
        assert_eq!(writer.lock_inner().next_seq, 2);
        Ok(())
    }

    #[test]
    fn payload_serialization_releases_event_log_lock() -> anyhow::Result<()> {
        struct ChecksLock<'a>(&'a TraceWriter);
        impl serde::Serialize for ChecksLock<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let _guard = self.0.inner.try_lock().map_err(serde::ser::Error::custom)?;
                serializer.serialize_str("payload")
            }
        }
        let temp = TempDir::new()?;
        let writer =
            TraceWriter::create(temp.path(), "trace".into(), "rollout".into(), "root".into())?;
        let payload =
            writer.write_json_payload(RawPayloadKind::ProtocolEvent, &ChecksLock(&writer))?;
        assert_eq!(
            std::fs::read_to_string(temp.path().join(payload.path))?,
            "\"payload\""
        );
        Ok(())
    }

    #[test]
    fn writer_records_payload_refs_and_replays_rollout_status() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".to_string(),
            "rollout-1".to_string(),
            "thread-root".to_string(),
        )?;

        writer.append(RawTraceEventPayload::RolloutStarted {
            trace_id: "trace-1".to_string(),
            root_thread_id: "thread-root".to_string(),
        })?;
        let metadata_payload = writer.write_json_payload(
            RawPayloadKind::ProtocolEvent,
            &json!({
                "source": "test",
                "model": "gpt-test",
            }),
        )?;
        writer.append(RawTraceEventPayload::ThreadStarted {
            thread_id: "thread-root".to_string(),
            agent_path: "/root".to_string(),
            metadata_payload: Some(metadata_payload.clone()),
        })?;
        writer.append(RawTraceEventPayload::CodexTurnStarted {
            codex_turn_id: "turn-1".to_string(),
            thread_id: "thread-root".to_string(),
        })?;
        let inference_request = writer.write_json_payload(
            RawPayloadKind::InferenceRequest,
            &json!({
                "model": "gpt-test",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }],
            }),
        )?;
        writer.append(RawTraceEventPayload::InferenceStarted {
            inference_call_id: "inference-1".to_string(),
            thread_id: "thread-root".to_string(),
            codex_turn_id: "turn-1".to_string(),
            model: "gpt-test".to_string(),
            provider_name: "test-provider".to_string(),
            request_payload: inference_request.clone(),
        })?;
        let inference_response = writer.write_json_payload(
            RawPayloadKind::InferenceResponse,
            &json!({
                "response_id": "resp-1",
                "output_items": [],
            }),
        )?;
        writer.append(RawTraceEventPayload::InferenceCompleted {
            inference_call_id: "inference-1".to_string(),
            response_id: Some("resp-1".to_string()),
            upstream_request_id: Some("req-1".to_string()),
            response_payload: Some(inference_response.clone()),
        })?;
        writer.append(RawTraceEventPayload::CodexTurnEnded {
            codex_turn_id: "turn-1".to_string(),
            status: ExecutionStatus::Completed,
        })?;
        writer.append(RawTraceEventPayload::RolloutEnded {
            status: RolloutStatus::Completed,
        })?;

        let rollout = replay_bundle(temp.path())?;

        assert_eq!(rollout.status, RolloutStatus::Completed);
        assert_eq!(rollout.root_thread_id, "thread-root");
        assert_eq!(rollout.threads["thread-root"].agent_path, "/root");
        assert_eq!(rollout.codex_turns["turn-1"].thread_id, "thread-root");
        assert_eq!(
            rollout.codex_turns["turn-1"].execution.status,
            ExecutionStatus::Completed,
        );
        assert_eq!(
            rollout.inference_calls["inference-1"].raw_request_payload_id,
            inference_request.raw_payload_id,
        );
        assert_eq!(
            rollout.inference_calls["inference-1"].raw_response_payload_id,
            Some(inference_response.raw_payload_id),
        );
        assert_eq!(
            rollout.raw_payloads[&metadata_payload.raw_payload_id].path,
            "payloads/1.json"
        );

        Ok(())
    }

    #[test]
    fn replay_keeps_the_complete_prefix_of_an_interrupted_log() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let writer =
            TraceWriter::create(temp.path(), "trace".into(), "rollout".into(), "root".into())?;
        for (thread_id, agent_path) in [("root", "/root"), ("child", "/root/é")] {
            writer.append(RawTraceEventPayload::ThreadStarted {
                thread_id: thread_id.into(),
                agent_path: agent_path.into(),
                metadata_payload: None,
            })?;
        }
        let log = temp.path().join("trace.jsonl");
        let bytes = std::fs::read(&log)?;
        // A crash or a live writer can leave the final append inside a UTF-8 character.
        let cut = bytes.iter().rposition(|byte| *byte == 0xC3).expect("é") + 1;
        std::fs::write(&log, &bytes[..cut])?;

        let replayed = replay_bundle(temp.path())?;
        assert_eq!(replayed.threads.keys().collect::<Vec<_>>(), vec!["root"]);

        // A terminated malformed record is corruption, not an unfinished append.
        std::fs::write(&log, [&bytes[..cut], b"\n"].concat())?;
        assert!(replay_bundle(temp.path()).is_err());
        Ok(())
    }

    #[test]
    fn best_effort_payload_write_warns_and_returns_none() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let writer = TraceWriter::create(
            temp.path(),
            "trace-1".to_string(),
            "rollout-1".to_string(),
            "thread-root".to_string(),
        )?;
        std::fs::remove_dir_all(temp.path().join(crate::bundle::PAYLOADS_DIR_NAME))?;

        assert!(
            writer
                .write_json_payload_best_effort(
                    RawPayloadKind::ProtocolEvent,
                    &json!({ "event": "missing-directory" }),
                )
                .is_none()
        );

        Ok(())
    }
}
