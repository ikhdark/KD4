//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::retry::backoff;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use http::StatusCode;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const MAX_RESPONSE_STREAM_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    LocalCompaction,
    RemoteCompactionV2,
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_retryable_response_stream_error(
    retries: &mut u64,
    max_retries: u64,
    err: CodexErr,
    client_session: &mut ModelClientSession,
    sess: &Session,
    turn_context: &TurnContext,
    request: ResponsesStreamRequest,
    cancellation_token: &CancellationToken,
) -> Result<(), CodexErr> {
    if !should_retry_response_stream(request, &err) {
        return Err(err);
    }

    if *retries >= max_retries
        && should_switch_fallback_transport(&err)
        && client_session.try_switch_fallback_transport(&turn_context.session_telemetry)
    {
        turn_context.turn_timing_state.record_model_fallback();
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
            }),
        )
        .await;
        // The loop itself supplies one immediate HTTPS fallback attempt. Keep the provider retry
        // budget exhausted so a failed fallback does not start a second full retry window.
        exhaust_retry_budget_for_http_fallback(retries, max_retries);
        return Ok(());
    }

    if *retries < max_retries {
        *retries += 1;
        let retry_count = *retries;
        let delay = response_stream_retry_delay(&err, retry_count);
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);

        // Surface retry information from the first attempt so a reconnect never looks frozen.
        sess.notify_stream_error(
            turn_context,
            format!("Reconnecting... {retry_count}/{max_retries}"),
            err,
        )
        .await;
        let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
        wait_for_retry_delay(delay, cancellation_token).await?;
        return Ok(());
    }

    Err(err)
}

fn exhaust_retry_budget_for_http_fallback(retries: &mut u64, max_retries: u64) {
    *retries = max_retries;
}

async fn wait_for_retry_delay(
    delay: Duration,
    cancellation_token: &CancellationToken,
) -> Result<(), CodexErr> {
    tokio::select! {
        _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

fn should_retry_response_stream(request: ResponsesStreamRequest, err: &CodexErr) -> bool {
    let _ = request;
    err.is_retryable()
        && !matches!(
            err,
            CodexErr::UnexpectedStatus(error) if error.status == StatusCode::UNAUTHORIZED
        )
}

fn should_switch_fallback_transport(err: &CodexErr) -> bool {
    matches!(
        err,
        CodexErr::RequestTimeout
            | CodexErr::ConnectionFailed(_)
            | CodexErr::ResponseStreamFailed(_)
            | CodexErr::UnexpectedStatus(_)
    )
}

fn response_stream_retry_delay(err: &CodexErr, retry_count: u64) -> Duration {
    let requested_or_backoff = match err {
        CodexErr::Stream(_, requested_delay) => {
            requested_delay.unwrap_or_else(|| backoff(retry_count))
        }
        _ => backoff(retry_count),
    };
    requested_or_backoff.min(MAX_RESPONSE_STREAM_RETRY_DELAY)
}

fn log_retry(
    request: ResponsesStreamRequest,
    turn_context: &TurnContext,
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    delay: Duration,
) {
    match request {
        ResponsesStreamRequest::Sampling => {
            warn!(
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );
        }
        ResponsesStreamRequest::LocalCompaction => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                compact_error = %err,
                "local compaction stream failed; retrying request after delay"
            );
        }
        ResponsesStreamRequest::RemoteCompactionV2 => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                compact_error = %err,
                "remote compaction v2 stream failed; retrying request after delay"
            );
        }
    }
}

#[cfg(test)]
#[path = "responses_retry_tests.rs"]
mod tests;
