use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionReason;
use codex_otel::SessionTelemetry;
use codex_protocol::error::CodexErr;
use tracing::warn;

/// Rejections of a completed compaction response. They are specific to the
/// model or checkpoint format that produced it, unlike the harness storage and
/// recovery failures that producers also report as `CodexErr::Fatal`.
const COMPACTION_CONTRACT_VIOLATIONS: &[&str] = &[
    "compaction completed without a checkpoint handoff",
    "compaction returned an incomplete excerpt, not a standalone checkpoint",
    "default compaction requires structured checkpoint sections",
    "incremental compaction handoff did not contain a recognized non-empty checkpoint section",
    "compaction handoff is incomplete; missing non-empty sections:",
    "compaction omitted a requested refreshed section",
    "incremental checkpoint needs a complete rebase to fit",
    "invalid structured compaction handoff:",
    "structured compaction requires exactly the six checkpoint sections",
    "structured compaction section ",
    "remote compaction v2 expected exactly one compaction output item",
    "remote compaction v2 returned an empty checkpoint",
];

pub(crate) fn is_compaction_contract_violation(error: &CodexErr) -> bool {
    matches!(error, CodexErr::Fatal(message)
        if COMPACTION_CONTRACT_VIOLATIONS
            .iter()
            .any(|violation| message.starts_with(violation)))
}

pub(crate) fn should_retry_with_current_model(error: &CodexErr) -> bool {
    match error {
        CodexErr::UnexpectedStatus(error) => error.status.is_server_error(),
        _ => {
            matches!(
                error,
                CodexErr::InvalidRequest(_)
                    | CodexErr::ContextWindowExceeded
                    | CodexErr::ServerOverloaded { .. }
                    | CodexErr::InternalServerError { .. }
            ) || is_compaction_contract_violation(error)
        }
    }
}

/// Switching compaction format can help only a request-specific rejection or
/// a rejected checkpoint. It cannot recover quota, credentials, local storage,
/// or ambiguous dispatch.
pub(crate) fn should_retry_with_local_compaction(error: &CodexErr) -> bool {
    match error {
        CodexErr::InvalidRequest(_)
        | CodexErr::ContextWindowExceeded
        | CodexErr::Stream(..)
        | CodexErr::InternalServerError { .. } => true,
        CodexErr::UnexpectedStatus(error) => error.status.is_server_error(),
        _ => is_compaction_contract_violation(error),
    }
}

pub(crate) fn record_model_fallback(
    session_telemetry: &SessionTelemetry,
    previous_model: &str,
    current_model: &str,
    reason: CompactionReason,
    implementation: CompactionImplementation,
    fallback_error: Option<&CodexErr>,
) {
    let reason_tag = match reason {
        CompactionReason::UserRequested => "user_requested",
        CompactionReason::ContextLimit => "context_limit",
        CompactionReason::ModelDownshift => "model_downshift",
        CompactionReason::CompHashChanged => "comp_hash_changed",
    };
    let implementation_tag = match implementation {
        CompactionImplementation::Responses => "responses",
        CompactionImplementation::ResponsesCompactionV2 => "responses_compaction_v2",
        CompactionImplementation::ResponsesCompact => "responses_compact",
    };
    let outcome = if fallback_error.is_none() {
        "succeeded"
    } else {
        "failed"
    };
    session_telemetry.counter(
        "codex.compaction.model_fallback",
        /*inc*/ 1,
        &[
            ("reason", reason_tag),
            ("implementation", implementation_tag),
            ("outcome", outcome),
        ],
    );
    warn!(
        previous_model,
        current_model,
        ?reason,
        ?implementation,
        outcome,
        ?fallback_error,
        "previous-model compaction failed; retried with current model"
    );
}
