//! Hot-path helpers for recording upstream remote compaction attempts.
//!
//! Remote compaction is a model-facing request with a different semantic role
//! from normal sampling. Keeping the no-op capable trace handle in this crate
//! lets `codex-core` record exact endpoint payloads without owning trace schema
//! details.

use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde::Serialize;

use crate::inference::serialize_trace_response_items;
use crate::model::AgentThreadId;
use crate::model::CodexTurnId;
use crate::model::CompactionId;
use crate::model::CompactionRequestId;
use crate::payload::RawPayloadKind;
use crate::raw_event::RawTraceEventContext;
use crate::raw_event::RawTraceEventPayload;
use crate::writer::TraceWriter;

static NEXT_COMPACTION_REQUEST: AtomicU64 = AtomicU64::new(1);

/// Turn-local remote compaction tracing context.
///
/// A compaction can retry its upstream request before installing one checkpoint. The context
/// owns the stable checkpoint ID; each request attempt gets a separate request ID.
#[derive(Clone, Debug)]
pub struct CompactionTraceContext {
    state: CompactionTraceContextState,
}

#[derive(Clone, Debug)]
enum CompactionTraceContextState {
    Disabled,
    Enabled(EnabledCompactionTraceContext),
}

#[derive(Clone, Debug)]
struct EnabledCompactionTraceContext {
    occurrence: Option<crate::TraceOccurrence>,
    writer: Arc<TraceWriter>,
    thread_id: AgentThreadId,
    codex_turn_id: CodexTurnId,
    compaction_id: CompactionId,
    model: String,
    provider_name: String,
}

/// One upstream request attempt made while computing a compaction checkpoint.
#[derive(Clone, Debug)]
pub struct CompactionTraceAttempt {
    state: CompactionTraceAttemptState,
}

#[derive(Clone, Debug)]
enum CompactionTraceAttemptState {
    Disabled,
    Enabled(EnabledCompactionTraceAttempt),
}

#[derive(Clone, Debug)]
struct EnabledCompactionTraceAttempt {
    context: EnabledCompactionTraceContext,
    compaction_request_id: CompactionRequestId,
    terminal_recorded: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct TracedCompactionCompleted<'a> {
    #[serde(serialize_with = "serialize_trace_response_items")]
    output_items: &'a [ResponseItem],
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<&'a CompactionResponseReceipt>,
}

/// Observed physical provider terminal, independent of checkpoint acceptance.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CompactionResponseReceipt {
    pub response_id: Option<String>,
    pub sampling_request_id: Option<String>,
    pub physical_attempt_id: Option<String>,
    pub token_usage: Option<TokenUsage>,
    pub provider_completed: bool,
}

/// History replacement checkpoint persisted when compaction installs new live history.
///
/// The checkpoint keeps compaction separate from ordinary sampling snapshots:
/// `input_history` is the live thread history selected for compaction, while
/// `replacement_history` is what future prompts may carry after the checkpoint.
#[derive(Serialize)]
pub struct CompactionCheckpointTracePayload<'a> {
    #[serde(serialize_with = "serialize_trace_response_items")]
    pub input_history: &'a [ResponseItem],
    #[serde(serialize_with = "serialize_trace_response_items")]
    pub replacement_history: &'a [ResponseItem],
}

impl CompactionTraceContext {
    /// Builds a context that accepts trace calls and records nothing.
    pub fn disabled() -> Self {
        Self {
            state: CompactionTraceContextState::Disabled,
        }
    }

    /// Returns whether compaction trace payloads will be recorded.
    ///
    /// Callers should branch on this only when preparing a checkpoint payload
    /// would otherwise require cloning history that the request path still owns.
    pub fn is_enabled(&self) -> bool {
        matches!(self.state, CompactionTraceContextState::Enabled(_))
    }

    /// Builds an enabled context for upstream attempts that compute one checkpoint.
    pub fn enabled(
        writer: Arc<TraceWriter>,
        thread_id: AgentThreadId,
        codex_turn_id: CodexTurnId,
        compaction_id: CompactionId,
        model: String,
        provider_name: String,
    ) -> Self {
        Self {
            state: CompactionTraceContextState::Enabled(EnabledCompactionTraceContext {
                occurrence: None,
                writer,
                thread_id,
                codex_turn_id,
                compaction_id,
                model,
                provider_name,
            }),
        }
    }

    /// Starts a new upstream attempt and records the exact compact endpoint request.
    pub fn start_attempt(&self, request: &impl Serialize) -> CompactionTraceAttempt {
        self.start_attempt_at(request, crate::TraceOccurrence::now())
    }

    pub fn start_attempt_at(
        &self,
        request: &impl Serialize,
        occurrence: crate::TraceOccurrence,
    ) -> CompactionTraceAttempt {
        let CompactionTraceContextState::Enabled(context) = &self.state else {
            return CompactionTraceAttempt::disabled();
        };

        let mut context = context.clone();
        context.occurrence = Some(occurrence);
        let attempt = CompactionTraceAttempt {
            state: CompactionTraceAttemptState::Enabled(EnabledCompactionTraceAttempt {
                context,
                compaction_request_id: next_compaction_request_id(),
                terminal_recorded: Arc::new(AtomicBool::new(false)),
            }),
        };
        if attempt.record_started(request) {
            attempt
        } else {
            CompactionTraceAttempt::disabled()
        }
    }

    /// Records the point where compacted history becomes the live thread history.
    ///
    /// The checkpoint belongs to the same semantic compaction lifecycle as the
    /// compact endpoint attempts, so the context reuses its stable compaction ID.
    pub fn record_installed(&self, checkpoint: &CompactionCheckpointTracePayload<'_>) {
        let CompactionTraceContextState::Enabled(context) = &self.state else {
            return;
        };
        let Some(checkpoint_payload) = context
            .writer
            .write_json_payload_best_effort(RawPayloadKind::CompactionCheckpoint, checkpoint)
        else {
            return;
        };

        let event_context = RawTraceEventContext {
            thread_id: Some(context.thread_id.clone()),
            codex_turn_id: Some(context.codex_turn_id.clone()),
        };
        context.writer.append_with_context_best_effort(
            event_context,
            RawTraceEventPayload::CompactionInstalled {
                compaction_id: context.compaction_id.clone(),
                checkpoint_payload,
            },
        );
    }
}

impl CompactionTraceAttempt {
    pub fn at_occurrence(&self, occurrence: crate::TraceOccurrence) -> Self {
        let mut attempt = self.clone();
        if let CompactionTraceAttemptState::Enabled(state) = &mut attempt.state {
            state.context.occurrence = Some(occurrence);
        }
        attempt
    }
    /// Builds an attempt that records nothing.
    fn disabled() -> Self {
        Self {
            state: CompactionTraceAttemptState::Disabled,
        }
    }

    fn record_started(&self, request: &impl Serialize) -> bool {
        let CompactionTraceAttemptState::Enabled(attempt) = &self.state else {
            return false;
        };
        let Some(request_payload) = write_json_payload_best_effort(
            &attempt.context.writer,
            RawPayloadKind::CompactionRequest,
            request,
        ) else {
            append_with_context_best_effort(
                &attempt.context,
                RawTraceEventPayload::CompactionRequestStartedWithoutPayload {
                    compaction_id: attempt.context.compaction_id.clone(),
                    compaction_request_id: attempt.compaction_request_id.clone(),
                    thread_id: attempt.context.thread_id.clone(),
                    codex_turn_id: attempt.context.codex_turn_id.clone(),
                    model: attempt.context.model.clone(),
                    provider_name: attempt.context.provider_name.clone(),
                },
            );
            return true;
        };

        append_with_context_best_effort(
            &attempt.context,
            RawTraceEventPayload::CompactionRequestStarted {
                compaction_id: attempt.context.compaction_id.clone(),
                compaction_request_id: attempt.compaction_request_id.clone(),
                thread_id: attempt.context.thread_id.clone(),
                codex_turn_id: attempt.context.codex_turn_id.clone(),
                model: attempt.context.model.clone(),
                provider_name: attempt.context.provider_name.clone(),
                request_payload,
            },
        );
        true
    }

    /// Records the non-streaming compact endpoint response payload.
    ///
    /// Compaction responses use the same response-item preservation rules as
    /// inference streams: traces are evidence, while normal ResponseItem
    /// serialization is shaped for future request construction.
    pub fn record_completed(&self, output_items: &[ResponseItem]) {
        self.record_completed_with_receipt(output_items, None);
    }

    pub fn record_completed_with_receipt(
        &self,
        output_items: &[ResponseItem],
        receipt: Option<&CompactionResponseReceipt>,
    ) {
        let Some(attempt) = self.take_terminal_attempt() else {
            return;
        };
        let response_payload = TracedCompactionCompleted {
            output_items,
            receipt,
        };
        let response_payload = write_json_payload_best_effort(
            &attempt.context.writer,
            RawPayloadKind::CompactionResponse,
            &response_payload,
        );

        append_with_context_best_effort(
            &attempt.context,
            RawTraceEventPayload::CompactionRequestCompleted {
                compaction_id: attempt.context.compaction_id.clone(),
                compaction_request_id: attempt.compaction_request_id.clone(),
                response_payload,
            },
        );
    }

    /// Records the compact endpoint result without forcing callers to branch on trace events.
    pub fn record_result<E: Display>(&self, result: Result<&[ResponseItem], E>) {
        match result {
            Ok(output_items) => self.record_completed(output_items),
            Err(err) => self.record_failed(err),
        }
    }

    pub fn record_result_with_receipt<E: Display>(
        &self,
        result: Result<&[ResponseItem], E>,
        receipt: Option<&CompactionResponseReceipt>,
    ) {
        match result {
            Ok(output_items) => self.record_completed_with_receipt(output_items, receipt),
            Err(error) => self.record_failed_with_receipt(error, receipt),
        }
    }

    /// Records pre-response failures from the compact endpoint.
    pub fn record_failed(&self, error: impl Display) {
        self.record_failed_with_receipt(error, None);
    }

    fn record_failed_with_receipt(
        &self,
        error: impl Display,
        receipt: Option<&CompactionResponseReceipt>,
    ) {
        let Some(attempt) = self.take_terminal_attempt() else {
            return;
        };
        let response_payload = receipt.and_then(|receipt| {
            write_json_payload_best_effort(
                &attempt.context.writer,
                RawPayloadKind::CompactionResponse,
                &TracedCompactionCompleted {
                    output_items: &[],
                    receipt: Some(receipt),
                },
            )
        });
        append_with_context_best_effort(
            &attempt.context,
            RawTraceEventPayload::CompactionRequestFailed {
                compaction_id: attempt.context.compaction_id.clone(),
                compaction_request_id: attempt.compaction_request_id.clone(),
                error: error.to_string(),
                response_payload,
            },
        );
    }

    pub fn is_terminal(&self) -> bool {
        match &self.state {
            CompactionTraceAttemptState::Disabled => true,
            CompactionTraceAttemptState::Enabled(attempt) => {
                attempt.terminal_recorded.load(Ordering::Acquire)
            }
        }
    }

    pub fn record_cancelled(&self, reason: impl Display) {
        let Some(attempt) = self.take_terminal_attempt() else {
            return;
        };
        append_with_context_best_effort(
            &attempt.context,
            RawTraceEventPayload::CompactionRequestCancelled {
                compaction_id: attempt.context.compaction_id.clone(),
                compaction_request_id: attempt.compaction_request_id.clone(),
                reason: reason.to_string(),
            },
        );
    }

    fn take_terminal_attempt(&self) -> Option<&EnabledCompactionTraceAttempt> {
        let CompactionTraceAttemptState::Enabled(attempt) = &self.state else {
            return None;
        };
        (!attempt.terminal_recorded.swap(true, Ordering::AcqRel)).then_some(attempt)
    }
}

fn next_compaction_request_id() -> CompactionRequestId {
    let ordinal = NEXT_COMPACTION_REQUEST.fetch_add(1, Ordering::Relaxed);
    format!("compaction_request:{ordinal}")
}

fn write_json_payload_best_effort(
    writer: &TraceWriter,
    kind: RawPayloadKind,
    payload: &impl Serialize,
) -> Option<crate::RawPayloadRef> {
    writer.write_json_payload_best_effort(kind, payload)
}

fn append_with_context_best_effort(
    context: &EnabledCompactionTraceContext,
    payload: RawTraceEventPayload,
) {
    let event_context = RawTraceEventContext {
        thread_id: Some(context.thread_id.clone()),
        codex_turn_id: Some(context.codex_turn_id.clone()),
    };
    if let Err(err) =
        context
            .writer
            .append_with_context_at(event_context, payload, context.occurrence.clone())
    {
        tracing::warn!("failed to append rollout compaction event: {err:#}");
    }
}

#[cfg(test)]
mod audit110_tests {
    use super::*;
    use crate::ExecutionStatus;
    use crate::replay_bundle;

    #[test]
    fn missing_request_payload_retains_compaction_lifecycle() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new()?;
        let writer = Arc::new(TraceWriter::create(
            temp.path(),
            "trace".into(),
            "rollout".into(),
            "root".into(),
        )?);
        writer.append(RawTraceEventPayload::ThreadStarted {
            thread_id: "root".into(),
            agent_path: "/root".into(),
            metadata_payload: None,
        })?;
        writer.append(RawTraceEventPayload::CodexTurnStarted {
            codex_turn_id: "turn".into(),
            thread_id: "root".into(),
        })?;
        std::fs::create_dir(temp.path().join("payloads/1.json"))?;
        let context = CompactionTraceContext::enabled(
            Arc::clone(&writer),
            "root".into(),
            "turn".into(),
            "compact".into(),
            "model".into(),
            "provider".into(),
        );
        let attempt = context.start_attempt(&serde_json::json!({"input": []}));
        attempt.record_completed(&[]);
        attempt.record_failed("duplicate terminal");
        let rollout = replay_bundle(temp.path())?;
        assert_eq!(rollout.compaction_requests.len(), 1);
        let request = rollout.compaction_requests.values().next().unwrap();
        assert_eq!(request.execution.status, ExecutionStatus::Completed);
        assert!(request.raw_request_payload_id.is_none());
        assert!(request.raw_response_payload_id.is_some());
        assert_eq!(rollout.payload_capture_gap_count, Some(1));
        assert!(rollout.compactions.is_empty());
        Ok(())
    }
}
