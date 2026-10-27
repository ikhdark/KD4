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
// Capacity needs time to recover, not the subsecond cadence used for a broken
// stream. Keep the provider's retry count and server-supplied deadlines intact.
const INITIAL_CAPACITY_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_CAPACITY_RETRY_DELAY: Duration = Duration::from_secs(60);
const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(15);
/// Bounds automatic recovery after the first failure, including account waits.
/// This is not a timeout on an accepted, potentially long-running generation.
const INTERACTIVE_RETRY_WAIT_BUDGET: Duration = Duration::from_secs(120);
const INTERACTIVE_RETRY_BUDGET_ERROR_CODE: &str = "interactive_retry_budget_exhausted";
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
/// `retries` tracks the bounded generation replay budget. Pre-dispatch connection
/// failures are tracked separately; interactive recovery shares one wait budget.
#[derive(Debug)]
pub(crate) struct ResponsesStreamRetryState {
    retries: u64,
    connection_retries: u64,
    connection_retry_delay: Duration,
    wait_deadline: Option<tokio::time::Instant>,
    account_wait_started: bool,
    /// Whether this request already spent its one WebSocket reconnect on a
    /// close that invited another connection. It also consumes `retries`:
    /// reconnect and HTTPS fallback share the post-dispatch replay budget.
    reconnected_after_close: bool,
    repaired_explicit_rejection: bool,
}

impl Default for ResponsesStreamRetryState {
    fn default() -> Self {
        Self {
            retries: 0,
            connection_retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
            wait_deadline: None,
            account_wait_started: false,
            reconnected_after_close: false,
            repaired_explicit_rejection: false,
        }
    }
}

impl ResponsesStreamRetryState {
    fn interactive_wait_deadline(
        &mut self,
        session_source: &SessionSource,
    ) -> Option<tokio::time::Instant> {
        if !is_interactive_recovery(session_source) {
            return None;
        }
        Some(*self.wait_deadline.get_or_insert_with(|| {
            tokio::time::Instant::now() + INTERACTIVE_RETRY_WAIT_BUDGET
        }))
    }

    /// Check before another attempt, without starting a timer for the initial request.
    pub(crate) fn check_retry_budget(
        &self,
        cancellation_token: &CancellationToken,
    ) -> Result<(), CodexErr> {
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        check_retry_wait_budget(tokio::time::Instant::now(), self.wait_deadline, false)
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
    let preconnect_failed = client_session.take_preconnect_failure_credit();
    // Do not mutate the session-wide transport choice or announce another
    // attempt after the turn owner has already cancelled this request.
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if !should_retry_response_stream(&err) {
        return Err(err);
    }
    let retry_after = err.retry_after();
    let server_busy = is_server_busy(&err);
    if server_busy {
        // A capacity rejection can leave the socket open and its sticky routing
        // token pointing at the rejecting backend. Retire both, including on
        // the final attempt, without changing the retry budget or server delay.
        client_session.replace_websocket_for_recovery();
    }

    let wait_deadline = retry_state.interactive_wait_deadline(&turn_context.session_source);
    check_retry_wait_budget(
        retry_after
            .map(RetryAfter::deadline)
            .unwrap_or_else(tokio::time::Instant::now),
        wait_deadline,
        retry_after.is_some(),
    )?;

    if matches!(&err, CodexErr::WebsocketRejected { .. }) {
        if retry_state.repaired_explicit_rejection {
            return Err(err);
        }
        retry_state.repaired_explicit_rejection = true;
        // Each typed refusal proves no sampling was accepted. Retire the
        // chain/socket so the next request carries full authoritative input.
        client_session.replace_websocket_for_recovery();
        if let Some(advice) = retry_after {
            wait_for_response_retry_deadline(
                advice.deadline(),
                wait_deadline,
                true,
                sess,
                turn_context,
                cancellation_token,
            )
            .await?;
        }
        return Ok(());
    }

    // A server that closed mid-response with "connect again" is not a reason to
    // abandon WebSockets for the session: rebuild the stream once first. Placed
    // ahead of the fallback branch because that branch switches transport on the
    // first stream-read failure, which would otherwise consume this case.
    if retry_state.retries < max_retries && should_reconnect_after_close(&err, retry_state) {
        retry_state.retries += 1;
        retry_state.reconnected_after_close = true;
        let delay = response_stream_retry_delay(&err, retry_state.retries);
        let retry_at = retry_after
            .map(RetryAfter::deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + delay);
        warn!(
            turn_id = %turn_context.sub_id,
            ?request,
            ?delay,
            error = %err,
            "provider closed the stream and invited a reconnect; rebuilding the websocket"
        );
        client_session.replace_websocket_for_recovery();
        sess.notify_stream_error(
            turn_context,
            format!("Reconnecting... the server closed the stream (next retry in {delay:.1?})"),
            err,
        )
        .await;
        wait_for_response_retry_deadline(
            retry_at,
            wait_deadline,
            retry_after.is_some(),
            sess,
            turn_context,
            cancellation_token,
        )
        .await?;
        return Ok(());
    }

    let wait_for_connection_recovery = should_wait_for_connection_recovery(
        &err,
        &turn_context.session_source,
        turn_context.provider.info(),
    );
    let failed_upgrade = is_websocket_upgrade_failure(&err);
    let pre_dispatch_failure = matches!(err, CodexErr::ConnectionFailed(_)) || failed_upgrade;
    // The ordinary dial already ran immediately. Account for a matching failed
    // speculative dial once, without inserting a backoff before transient
    // recovery or granting another retry window after persistent failure.
    if preconnect_failed && max_retries > 0 {
        if pre_dispatch_failure {
            retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
        } else {
            retry_state.retries = retry_state.retries.saturating_add(1);
        }
    }
    // ConnectionFailed is pre-dispatch: try healthy HTTPS before spending a
    // whole provider retry window on blocked upgrades. HTTP upgrade failures
    // fall back after their separate connection budget, even when it is zero.
    // A possibly accepted generation may fall back only while replay is allowed.
    let fallback_due = matches!(err, CodexErr::ConnectionFailed(_))
        || (failed_upgrade && retry_state.connection_retries >= max_retries)
        || (matches!(err, CodexErr::ResponseStreamFailed(_)) && retry_state.retries < max_retries);
    if fallback_due
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
        // A post-dispatch HTTPS replay spends the same budget as a reconnect.
        // Proven pre-dispatch connection recovery does not spend that budget.
        if !pre_dispatch_failure {
            retry_state.retries += 1;
        }
        if let Some(advice) = retry_after {
            wait_for_response_retry_deadline(
                advice.deadline(),
                wait_deadline,
                true,
                sess,
                turn_context,
                cancellation_token,
            )
            .await?;
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
        // A lost connection does not burn the provider attempt count, but the shared
        // interactive deadline prevents an outage from keeping this request alive forever.
        sess.notify_stream_error(
            turn_context,
            format!(
                "Reconnecting... waiting for network (attempt {}, next retry in {retry_delay:.1?})",
                retry_state.connection_retries,
            ),
            err,
        )
        .await;
        wait_for_response_retry_deadline(
            tokio::time::Instant::now() + retry_delay,
            wait_deadline,
            false,
            sess,
            turn_context,
            cancellation_token,
        )
        .await?;
        retry_state.connection_retry_delay = next_connection_retry_delay(retry_delay);
        // This branch only handles proven pre-dispatch connection failures. An HTTP-only
        // outage must not pin this same request to HTTP forever after WebSockets recover.
        // Keep ordinary post-dispatch retries sticky and preserve the shared cooldown.
        client_session.refresh_transport_after_cooldown();
        return Ok(());
    }

    // Failed connections and HTTP upgrades never dispatched response.create.
    // They cannot consume the later accepted generation's replay budget.
    let retries = if pre_dispatch_failure {
        &mut retry_state.connection_retries
    } else {
        &mut retry_state.retries
    };
    if *retries < max_retries {
        *retries += 1;
        let retry_count = *retries;
        let delay = response_stream_retry_delay(&err, retry_count);
        let retry_at = retry_after
            .map(RetryAfter::deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + delay);
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);

        // Surface retry information from the first attempt so a reconnect never looks frozen.
        let status = if server_busy {
            "Server is busy; retrying"
        } else {
            "Reconnecting"
        };
        sess.notify_stream_error(
            turn_context,
            format!("{status}... {retry_count}/{max_retries} (next retry in {delay:.1?})"),
            err,
        )
        .await;
        wait_for_response_retry_deadline(
            retry_at,
            wait_deadline,
            retry_after.is_some(),
            sess,
            turn_context,
            cancellation_token,
        )
        .await?;
        return Ok(());
    }

    Err(err)
}

/// Decides whether a mid-response close should rebuild the WebSocket instead of
/// abandoning the transport for the rest of the session.
///
/// Applies once per request, and only to a close whose code invites another
/// connection. Recovery backs off so a restarting or overloaded backend has
/// time to recover; a second close of the same request falls through to the
/// ordinary fallback path. A close code that rejects the request (policy,
/// protocol, payload) is never reconnected, since a new socket would only repeat
/// the rejection.
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
/// Post-dispatch silence is not evidence of an offline connection: the provider
/// may already be generating. It must consume the bounded retry budget rather
/// than regenerating indefinitely under the offline wait policy.
fn should_wait_for_connection_recovery(
    err: &CodexErr,
    session_source: &SessionSource,
    provider: &ModelProviderInfo,
) -> bool {
    let recoverable_stall = matches!(err, CodexErr::ConnectionFailed(_));
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

/// Recovery identity resolved for the dispatched attempt. Do not derive Debug: API keys
/// are compared only in memory and must never appear in retry diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum AccountChangeIdentity {
    Account(String),
    ApiKey(String),
    Headers(Vec<(String, Vec<u8>)>),
}

impl AccountChangeIdentity {
    pub(crate) fn from_auth(auth: &codex_login::CodexAuth) -> Option<Self> {
        if let codex_login::CodexAuth::Headers(headers) = auth {
            return Some(headers.headers().get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(|account| Self::Account(account.to_owned()))
                .unwrap_or_else(|| {
                    let mut values = headers.headers().iter()
                        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
                        .collect::<Vec<_>>();
                    values.sort_unstable();
                    Self::Headers(values)
                }));
        }
        // Refreshing tokens for the same account does not reset its usage limit.
        auth.get_account_id()
            .map(Self::Account)
            .or_else(|| auth.api_key().map(|key| Self::ApiKey(key.to_owned())))
    }
}

/// Why a model request waits for the account to change.
pub(crate) enum AccountChangeWait<'a> {
    UsageLimit(
        &'a UsageLimitReachedError,
        Option<&'a AccountChangeIdentity>,
    ),
    MissingCredentials,
}

/// Waits, without spending the stream retry budget, for the provider's
/// credentials to change (a login or account switch) or a usage limit to reset.
///
/// Returns `Ok(true)` when the request should be sent again and `Ok(false)` when
/// the wait does not apply. Account recovery shares the request's elapsed retry
/// budget with transport/capacity recovery; expiry stops automatic recovery.
/// Interactive sessions and the agents they spawn wait; other sessions fail
/// fast for their callers.
pub(crate) async fn wait_for_account_change(
    reason: AccountChangeWait<'_>,
    retry_state: &mut ResponsesStreamRetryState,
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
) -> Result<bool, CodexErr> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if !is_interactive_recovery(&turn_context.session_source) {
        return Ok(false);
    }
    let Some(auth_manager) = turn_context.provider.auth_manager() else {
        return Ok(false);
    };
    // Subscribe before rechecking the predicate: a login between the caller's
    // observation and this subscription is already seen by a new receiver.
    let mut auth_changes = auth_manager.auth_change_receiver();
    let current_account = || {
        auth_manager
            .auth_cached()
            .and_then(|auth| AccountChangeIdentity::from_auth(&auth))
    };
    let failed_account = match &reason {
        AccountChangeWait::UsageLimit(_, account) => account.cloned().or_else(current_account),
        AccountChangeWait::MissingCredentials => None,
    };
    if matches!(reason, AccountChangeWait::MissingCredentials)
        && !turn_context.provider.missing_required_credentials()
    {
        return Ok(true);
    }
    let now = tokio::time::Instant::now();
    let first_wait = !std::mem::replace(&mut retry_state.account_wait_started, true);
    let Some(deadline) = retry_state.interactive_wait_deadline(&turn_context.session_source) else {
        return Ok(false);
    };
    retry_state.check_retry_budget(cancellation_token)?;
    if matches!(reason, AccountChangeWait::UsageLimit(..))
        && current_account().is_some_and(|account| Some(account) != failed_account)
        && !turn_context.provider.missing_required_credentials()
    {
        return Ok(true);
    }
    // A usage limit that resets inside the bound ends the wait at the reset.
    let known_reset_at = match &reason {
        AccountChangeWait::UsageLimit(error, _) => error.resets_at.map(|resets_at| {
            let delay = (resets_at - chrono::Utc::now())
                .to_std()
                .unwrap_or_default();
            // A due reset is evidence, not an absent timestamp. Only the
            // first wait gets an immediate retry; repeated stale responses
            // retain the logical deadline and a cancellable reload backoff.
            now + if first_wait {
                delay
            } else {
                delay.max(ACCOUNT_CHANGE_RELOAD_INTERVAL)
            }
        }),
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
        AccountChangeWait::UsageLimit(error, _) => format!(
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
            changed = auth_changes.changed() => {
                if changed.is_err() { return Ok(false); }
                if !turn_context.provider.missing_required_credentials()
                    && (matches!(reason, AccountChangeWait::MissingCredentials)
                        || current_account().is_some_and(|account| Some(account) != failed_account))
                {
                    return Ok(true);
                }
            }
            _ = tokio::time::sleep_until(reset_at.unwrap_or(deadline)) => {
                retry_state.check_retry_budget(cancellation_token)?;
                return Ok(reset_at.is_some());
            }
            _ = reload.tick() => {
                // The source owner enforces external TTL and generation checks.
                // Keep one refresh alive without masking cancellation, reset or
                // an account switch published while metadata resolution stalls.
                let refresh = auth_manager.reload();
                tokio::pin!(refresh);
                loop {
                    tokio::select! {
                        biased;
                        _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                        _ = tokio::time::sleep_until(reset_at.unwrap_or(deadline)) => {
                            retry_state.check_retry_budget(cancellation_token)?;
                            return Ok(reset_at.is_some());
                        }
                        changed = auth_changes.changed() => {
                            if changed.is_err() { return Ok(false); }
                            if !turn_context.provider.missing_required_credentials()
                                && (matches!(reason, AccountChangeWait::MissingCredentials)
                                    || current_account().is_some_and(|account| Some(account) != failed_account))
                            {
                                return Ok(true);
                            }
                        }
                        _ = &mut refresh => break,
                    }
                }
            }
        }
    }
}

fn is_interactive_recovery(session_source: &SessionSource) -> bool {
    matches!(
        session_source,
        SessionSource::Cli
            | SessionSource::VSCode
            | SessionSource::Mcp
            | SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
    )
}

fn retry_wait_budget_error(reason: &str) -> CodexErr {
    CodexErr::ProviderFailure {
        code: Some(INTERACTIVE_RETRY_BUDGET_ERROR_CODE.to_string()),
        message: format!(
            "{reason} Automatic recovery is limited to {} seconds per model request. The selected model has not changed. Retry this model later, or select a different model manually.",
            INTERACTIVE_RETRY_WAIT_BUDGET.as_secs(),
        ),
    }
}

/// Context retirement may recover an ordinary compaction failure, not a deliberate
/// stop that handed retry/model choice back to the user.
pub(crate) fn is_retry_wait_budget_exhausted(error: &CodexErr) -> bool {
    match error {
        CodexErr::TerminalResponse { source, .. } => is_retry_wait_budget_exhausted(source),
        CodexErr::ProviderFailure { code, .. } => {
            code.as_deref() == Some(INTERACTIVE_RETRY_BUDGET_ERROR_CODE)
        }
        _ => false,
    }
}

fn check_retry_wait_budget(
    retry_at: tokio::time::Instant,
    wait_deadline: Option<tokio::time::Instant>,
    provider_directed: bool,
) -> Result<(), CodexErr> {
    let Some(wait_deadline) = wait_deadline else {
        return Ok(());
    };
    let now = tokio::time::Instant::now();
    if now >= wait_deadline {
        return Err(retry_wait_budget_error("Automatic retry waiting budget exhausted."));
    }
    if provider_directed && retry_at > wait_deadline {
        let seconds = retry_at.saturating_duration_since(now).as_millis().div_ceil(1000);
        return Err(retry_wait_budget_error(&format!(
            "The provider requires waiting at least {seconds} seconds before retrying this model. Automatic retry stopped instead of retrying before that deadline."
        )));
    }
    Ok(())
}

/// A compaction fallback is still a provider retry, not permission to ignore its delay.
pub(crate) async fn wait_for_response_retry_after(
    error: &CodexErr,
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
) -> Result<(), CodexErr> {
    let Some(advice) = error.retry_after() else {
        return Ok(());
    };
    let mut retry_state = ResponsesStreamRetryState::default();
    let wait_deadline = retry_state.interactive_wait_deadline(&turn_context.session_source);
    wait_for_response_retry_deadline(
        advice.deadline(),
        wait_deadline,
        true,
        sess,
        turn_context,
        cancellation_token,
    )
    .await
}

async fn wait_for_response_retry_deadline(
    retry_at: tokio::time::Instant,
    wait_deadline: Option<tokio::time::Instant>,
    provider_directed: bool,
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
) -> Result<(), CodexErr> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    check_retry_wait_budget(retry_at, wait_deadline, provider_directed)?;
    let delay = retry_at.saturating_duration_since(tokio::time::Instant::now());
    if provider_directed && delay > MAX_CONNECTION_RETRY_DELAY {
        let seconds = delay.as_millis().div_ceil(1000);
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            _ = sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Provider-directed retry wait: {seconds} seconds before retrying the current model. Cancel the turn to stop waiting; to use another model, cancel and select it manually. The harness will not switch models automatically."
                    ),
                }),
            ) => {}
        }
    }
    let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
    let deadline = wait_deadline.map_or(retry_at, |limit| retry_at.min(limit));
    wait_for_retry_deadline(deadline, cancellation_token).await?;
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    check_retry_wait_budget(tokio::time::Instant::now(), wait_deadline, false)
}

async fn wait_for_retry_deadline(
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

/// Only connect_websocket attaches a ws(s) URL to an HTTP status error.
/// Established WebSocket error frames carry no URL; ordinary HTTP failures
/// carry http(s). Preserve that provenance and its Retry-After/diagnostics
/// instead of treating a rejected upgrade as an accepted generation.
fn is_websocket_upgrade_failure(err: &CodexErr) -> bool {
    matches!(
        err,
        CodexErr::UnexpectedStatus(error)
            if error.status.is_server_error()
                && error.url.as_deref().is_some_and(|url| {
                    url.starts_with("ws://") || url.starts_with("wss://")
                })
    )
}

fn is_server_busy(err: &CodexErr) -> bool {
    matches!(err, CodexErr::ServerOverloaded { .. })
        || matches!(
            err,
            CodexErr::UnexpectedStatus(error)
                if error.status == StatusCode::TOO_MANY_REQUESTS
                    || error.status == StatusCode::SERVICE_UNAVAILABLE
        )
}

fn response_stream_retry_delay(err: &CodexErr, retry_count: u64) -> Duration {
    // Server delays are a minimum wait, not a suggestion to retry sooner.
    // Only our own exponential backoff is capped; all waits remain cancellable.
    if let Some(advice) = err.retry_after() {
        return advice.remaining_delay();
    }
    if is_server_busy(err)
        || matches!(err, CodexErr::ResponseStreamFailed(failed) if failed.reconnectable_close)
    {
        // Reuse the shared jitter implementation so simultaneous agents do not
        // retry in lockstep after capacity rejection or a server restart. A
        // close is not proof that a replacement backend is ready to accept work.
        // No new retry loop or independent budget is added.
        return codex_client::capped_backoff(
            INITIAL_CAPACITY_RETRY_DELAY,
            retry_count,
            MAX_CAPACITY_RETRY_DELAY,
        );
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
