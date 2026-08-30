//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::retry::backoff;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::WarningEvent;
use http::StatusCode;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const MAX_RESPONSE_STREAM_RETRY_DELAY: Duration = Duration::from_secs(5);
const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    LocalCompaction,
    RemoteCompactionV2,
}

/// Retry bookkeeping for one Responses stream loop.
///
/// `retries` tracks the bounded provider retry budget. Connection-loss waits are
/// tracked separately because they must not consume that budget.
#[derive(Debug)]
pub(crate) struct ResponsesStreamRetryState {
    retries: u64,
    connection_retries: u64,
    connection_retry_delay: Duration,
}

impl Default for ResponsesStreamRetryState {
    fn default() -> Self {
        Self {
            retries: 0,
            connection_retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
        }
    }
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_retryable_response_stream_error(
    retry_state: &mut ResponsesStreamRetryState,
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

    if should_wait_for_connection_recovery(
        request,
        &err,
        &turn_context.session_source,
        turn_context.provider.info(),
    ) {
        let retry_delay = retry_state.connection_retry_delay;
        retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
        warn!(
            turn_id = %turn_context.sub_id,
            connection_retries = retry_state.connection_retries,
            ?retry_delay,
            sampling_error = %err,
            "stream connection failed; waiting for the network to recover"
        );
        // Deliberately does not touch `retry_state.retries`: a lost connection must not
        // burn the bounded provider retry budget, so the turn survives sleep/wake and
        // VPN churn instead of failing after `max_retries` quick attempts.
        sess.notify_stream_error(turn_context, "Reconnecting... waiting for network", err)
            .await;
        let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
        wait_for_retry_delay(retry_delay, cancellation_token).await?;
        retry_state.connection_retry_delay = next_connection_retry_delay(retry_delay);
        return Ok(());
    }

    if retry_state.retries >= max_retries
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
        exhaust_retry_budget_for_http_fallback(&mut retry_state.retries, max_retries);
        return Ok(());
    }

    if retry_state.retries < max_retries {
        retry_state.retries += 1;
        let retry_count = retry_state.retries;
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

/// Decides whether a failed stream should wait for the network instead of
/// spending the bounded provider retry budget.
///
/// Restricted to user-facing sampling turns: compaction requests stay bounded so
/// they cannot stall a turn, internal sessions must fail fast for their callers,
/// and Amazon Bedrock reports unrelated failures through the same error class.
fn should_wait_for_connection_recovery(
    request: ResponsesStreamRequest,
    err: &CodexErr,
    session_source: &SessionSource,
    provider: &ModelProviderInfo,
) -> bool {
    matches!(request, ResponsesStreamRequest::Sampling)
        && matches!(err, CodexErr::ConnectionFailed(_))
        && !session_source.is_internal()
        && !provider.is_amazon_bedrock()
}

fn next_connection_retry_delay(delay: Duration) -> Duration {
    delay.saturating_mul(2).min(MAX_CONNECTION_RETRY_DELAY)
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
    match err {
        CodexErr::RequestTimeout
        | CodexErr::ConnectionFailed(_)
        | CodexErr::ResponseStreamFailed(_) => true,
        CodexErr::UnexpectedStatus(error) => error.status.is_server_error(),
        _ => false,
    }
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
