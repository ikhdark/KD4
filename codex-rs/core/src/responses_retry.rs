//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::retry::backoff_with_cap;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_http_client::RetryAfter;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::UsageLimitReachedError;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::WarningEvent;
use http::StatusCode;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const MAX_RESPONSE_STREAM_RETRY_DELAY: Duration = Duration::from_secs(5);
const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Bounds one request's wait for a login, account switch, or usage-limit reset.
const ACCOUNT_CHANGE_WAIT: Duration = Duration::from_secs(15 * 60);
/// Bounds the wait when the limit provably cannot reset inside the full bound,
/// so only a human account switch could help. Long enough to catch a switch
/// already under way, short enough not to stall a turn that cannot recover.
const ACCOUNT_SWITCH_ONLY_WAIT: Duration = Duration::from_secs(60);
/// Stored credentials are re-read at this interval while waiting.
const ACCOUNT_CHANGE_RELOAD_INTERVAL: Duration = Duration::from_secs(10);

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
    /// Whether this request already spent its one WebSocket reconnect on a
    /// close that invited another connection. Separate from `retries` so the
    /// reconnect does not shrink the provider budget, and per-request so a
    /// server restarting repeatedly cannot reconnect without bound.
    reconnected_after_close: bool,
}

impl Default for ResponsesStreamRetryState {
    fn default() -> Self {
        Self {
            retries: 0,
            connection_retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
            reconnected_after_close: false,
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
    // Do not mutate the session-wide transport choice or announce another
    // attempt after the turn owner has already cancelled this request.
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if !should_retry_response_stream(&err) {
        return Err(err);
    }
    let retry_after = err.retry_after();

    // A server that closed mid-response with "connect again" is not a reason to
    // abandon WebSockets for the session: rebuild the stream once first. Placed
    // ahead of the fallback branch because that branch switches transport on the
    // first stream-read failure, which would otherwise consume this case.
    if should_reconnect_after_close(&err, retry_state) {
        retry_state.reconnected_after_close = true;
        warn!(
            turn_id = %turn_context.sub_id,
            ?request,
            error = %err,
            "provider closed the stream and invited a reconnect; rebuilding the websocket"
        );
        client_session.replace_websocket_after_reconnectable_close();
        sess.notify_stream_error(
            turn_context,
            "Reconnecting... the server closed the stream".to_string(),
            err,
        )
        .await;
        return Ok(());
    }

    let wait_for_connection_recovery = should_wait_for_connection_recovery(
        &err,
        &turn_context.session_source,
        turn_context.provider.info(),
    );
    // Connection recovery has its own retry counter. It must still reach the
    // HTTPS fallback when the WebSocket endpoint remains unavailable.
    let retry_budget_exhausted = retry_state.retries >= max_retries
        || (wait_for_connection_recovery && retry_state.connection_retries >= max_retries);
    // Each new model request resets the retry budget. Switch on the first stream-read
    // failure so intermittent WebSocket drops cannot keep replaying work at "1/N" indefinitely.
    if (matches!(err, CodexErr::ResponseStreamFailed(_)) || retry_budget_exhausted)
        && should_switch_fallback_transport(&err)
        && client_session
            .try_switch_fallback_transport_after_error(&turn_context.session_telemetry, &err)
    {
        turn_context.turn_timing_state.record_model_fallback();
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
            }),
        )
        .await;
        // The loop supplies one immediate HTTPS attempt. Early fallback keeps the remaining
        // budget; exhausted recovery must not start a second full retry window.
        if retry_budget_exhausted {
            exhaust_retry_budget_for_http_fallback(&mut retry_state.retries, max_retries);
        }
        if let Some(advice) = retry_after {
            let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
            wait_for_retry_deadline(advice.deadline(), cancellation_token).await?;
        }
        return Ok(());
    }

    if wait_for_connection_recovery {
        let retry_delay = retry_state.connection_retry_delay;
        retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
        warn!(
            turn_id = %turn_context.sub_id,
            ?request,
            connection_retries = retry_state.connection_retries,
            ?retry_delay,
            error = %err,
            "stream connection failed; waiting for the network to recover"
        );
        // Deliberately does not touch `retry_state.retries`: a lost connection must not
        // burn the bounded provider retry budget, so the turn survives sleep/wake and
        // VPN churn instead of failing after `max_retries` quick attempts.
        sess.notify_stream_error(
            turn_context,
            format!(
                "Reconnecting... waiting for network (attempt {}, next retry in {retry_delay:.1?})",
                retry_state.connection_retries,
            ),
            err,
        )
        .await;
        let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
        wait_for_retry_delay(retry_delay, cancellation_token).await?;
        retry_state.connection_retry_delay = next_connection_retry_delay(retry_delay);
        return Ok(());
    }

    if retry_state.retries < max_retries {
        retry_state.retries += 1;
        let retry_count = retry_state.retries;
        let delay = response_stream_retry_delay(&err, retry_count);
        let retry_at = retry_after
            .map(RetryAfter::deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + delay);
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);

        // Surface retry information from the first attempt so a reconnect never looks frozen.
        sess.notify_stream_error(
            turn_context,
            format!("Reconnecting... {retry_count}/{max_retries} (next retry in {delay:.1?})"),
            err,
        )
        .await;
        let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
        wait_for_retry_deadline(retry_at, cancellation_token).await?;
        return Ok(());
    }

    Err(err)
}

fn exhaust_retry_budget_for_http_fallback(retries: &mut u64, max_retries: u64) {
    *retries = max_retries;
}

/// Decides whether a mid-response close should rebuild the WebSocket instead of
/// abandoning the transport for the rest of the session.
///
/// Applies once per request, and only to a close whose code invites another
/// connection. The reconnect is immediate and unbacked-off: the server has
/// already answered, so there is nothing to wait for, and a second close of the
/// same request falls through to the ordinary fallback path. A close code that
/// rejects the request (policy, protocol, payload) is never reconnected, since
/// a new socket would only repeat the rejection.
fn should_reconnect_after_close(err: &CodexErr, state: &ResponsesStreamRetryState) -> bool {
    !state.reconnected_after_close
        && matches!(err, CodexErr::ResponseStreamFailed(failed) if failed.reconnectable_close)
}

/// Decides whether a failed stream should wait for the network instead of
/// spending the bounded provider retry budget.
///
/// Covers compaction as well as sampling: compaction shares the sampling
/// transport, so an outage that outlasts its smaller budget would fail the turn
/// even though the next sampling request would wait for the network anyway.
/// Restricted to user-facing sessions: internal sessions must fail fast for
/// their callers, and Amazon Bedrock reports unrelated failures through the
/// same error class.
///
/// A post-connect idle timeout qualifies for the same treatment: the transport
/// was healthy and the provider simply went silent, which is the sleep/wake and
/// VPN-churn case this loop exists for. Charging it to the provider budget
/// instead (5 attempts by default, 2 for compaction, each capped at a 5 s
/// backoff) failed the turn after a few quick attempts on a link that would
/// have recovered. The flag is set only for silence, so a genuinely broken or
/// corrupt stream keeps failing fast.
fn should_wait_for_connection_recovery(
    err: &CodexErr,
    session_source: &SessionSource,
    provider: &ModelProviderInfo,
) -> bool {
    let recoverable_stall = match err {
        CodexErr::ConnectionFailed(_) => true,
        CodexErr::ResponseStreamFailed(failed) => failed.idle_timeout,
        _ => false,
    };
    recoverable_stall
        && matches!(
            session_source,
            SessionSource::Cli | SessionSource::VSCode | SessionSource::Mcp
        )
        && !provider.is_amazon_bedrock()
}

fn next_connection_retry_delay(delay: Duration) -> Duration {
    delay.saturating_mul(2).min(MAX_CONNECTION_RETRY_DELAY)
}

/// Why a model request waits for the account to change.
pub(crate) enum AccountChangeWait<'a> {
    UsageLimit(&'a UsageLimitReachedError),
    MissingCredentials,
}

/// Waits, without spending the stream retry budget, for the provider's
/// credentials to change (a login or account switch) or a usage limit to reset.
///
/// Returns `Ok(true)` when the request should be sent again and `Ok(false)` when
/// the wait does not apply or its bound has expired, so the caller keeps the
/// original outcome. One deadline bounds every wait of the same request.
/// Interactive sessions and the agents they spawn wait; other sessions fail
/// fast for their callers.
pub(crate) async fn wait_for_account_change(
    reason: AccountChangeWait<'_>,
    deadline: &mut Option<tokio::time::Instant>,
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
) -> Result<bool, CodexErr> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if !waits_for_account_change(&turn_context.session_source) {
        return Ok(false);
    }
    let Some(auth_manager) = turn_context.provider.auth_manager() else {
        return Ok(false);
    };
    // Subscribe before rechecking the predicate: a login between the caller's
    // observation and this subscription is already seen by a new receiver.
    let mut auth_changes = auth_manager.auth_change_receiver();
    if matches!(reason, AccountChangeWait::MissingCredentials)
        && !turn_context.provider.missing_required_credentials()
    {
        return Ok(true);
    }
    let now = tokio::time::Instant::now();
    let deadline = *deadline.get_or_insert(now + ACCOUNT_CHANGE_WAIT);
    let Some(_) = deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
    else {
        return Ok(false);
    };
    // A usage limit that resets inside the bound ends the wait at the reset.
    let known_reset_at = match &reason {
        AccountChangeWait::UsageLimit(error) => error
            .resets_at
            .and_then(|resets_at| (resets_at - chrono::Utc::now()).to_std().ok())
            .map(|delay| now + delay),
        AccountChangeWait::MissingCredentials => None,
    };
    let reset_at = known_reset_at.filter(|reset_at| *reset_at < deadline);
    // A reset beyond the bound cannot end this wait, so the full bound would be
    // spent on an account switch alone. Keep a short grace for a switch already
    // in progress instead of blocking the turn for the whole window.
    let deadline = if reset_at.is_none() && known_reset_at.is_some() {
        deadline.min(now + ACCOUNT_SWITCH_ONLY_WAIT)
    } else {
        deadline
    };
    let Some(remaining) = deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
    else {
        return Ok(false);
    };
    let minutes = remaining.as_secs().div_ceil(60);
    let message = match reason {
        AccountChangeWait::UsageLimit(error) => format!(
            "{error} Waiting up to {minutes} min for an account switch{} before retrying this request; interrupt the turn to stop waiting.",
            if reset_at.is_some() {
                " or the limit reset"
            } else if known_reset_at.is_some() {
                " (the reset is further out than this request can wait, so only an account switch would help)"
            } else {
                ""
            }
        ),
        AccountChangeWait::MissingCredentials => format!(
            "No model credentials are loaded (logged out or switching accounts). Waiting up to {minutes} min for a login before sending this request; interrupt the turn to stop waiting."
        ),
    };
    warn!(turn_id = %turn_context.sub_id, "{message}");
    sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
        .await;
    let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
    let mut reload = tokio::time::interval_at(
        now + ACCOUNT_CHANGE_RELOAD_INTERVAL,
        ACCOUNT_CHANGE_RELOAD_INTERVAL,
    );
    loop {
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            changed = auth_changes.changed() => return Ok(changed.is_ok()),
            _ = tokio::time::sleep_until(reset_at.unwrap_or(deadline)) => {
                return Ok(reset_at.is_some());
            }
            _ = reload.tick() => {
                // A login by another process changes only the stored credentials.
                // External auth resolves its source on every request instead.
                if !auth_manager.has_external_auth() {
                    auth_manager.reload().await;
                }
            }
        }
    }
}

fn waits_for_account_change(session_source: &SessionSource) -> bool {
    matches!(
        session_source,
        SessionSource::Cli
            | SessionSource::VSCode
            | SessionSource::Mcp
            | SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
    )
}

async fn wait_for_retry_delay(
    delay: Duration,
    cancellation_token: &CancellationToken,
) -> Result<(), CodexErr> {
    wait_for_retry_deadline(tokio::time::Instant::now() + delay, cancellation_token).await
}

pub(crate) async fn wait_for_retry_deadline(
    deadline: tokio::time::Instant,
    cancellation_token: &CancellationToken,
) -> Result<(), CodexErr> {
    tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
        _ = tokio::time::sleep_until(deadline) => Ok(()),
    }
}

fn should_retry_response_stream(err: &CodexErr) -> bool {
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
    // Server delays are a minimum wait, not a suggestion to retry sooner.
    // Only our own exponential backoff is capped; all waits remain cancellable.
    if let Some(advice) = err.retry_after() {
        return advice.remaining_delay();
    }
    backoff_with_cap(retry_count, MAX_RESPONSE_STREAM_RETRY_DELAY)
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
                turn_id = %turn_context.sub_id,
                sampling_error = %err,
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
