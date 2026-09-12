use std::sync::Arc;
use std::sync::Weak;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use codex_app_server_protocol::CurrentTimeReadParams;
use codex_app_server_protocol::CurrentTimeReadResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequestPayload;
use codex_core::SleepFuture;
use codex_core::TimeFuture;
use codex_core::TimeProvider;
use codex_protocol::ThreadId;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ThreadStateManager;

const CURRENT_TIME_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CURRENT_TIME_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) fn app_server_time_provider(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
) -> Arc<dyn TimeProvider> {
    Arc::new(AppServerTimeProvider {
        outgoing: Arc::downgrade(&outgoing),
        thread_state_manager,
    })
}

struct AppServerTimeProvider {
    outgoing: Weak<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
}

impl TimeProvider for AppServerTimeProvider {
    fn current_time(&self, thread_id: ThreadId) -> TimeFuture<'_> {
        let outgoing = self.outgoing.clone();
        let thread_state_manager = self.thread_state_manager.clone();
        Box::pin(async move {
            let outgoing = outgoing
                .upgrade()
                .context("app-server current-time provider is unavailable")?;
            request_current_time(outgoing, thread_state_manager, thread_id).await
        })
    }

    fn sleep(&self, thread_id: ThreadId, duration: Duration) -> SleepFuture<'_> {
        let outgoing = self.outgoing.clone();
        let thread_state_manager = self.thread_state_manager.clone();
        Box::pin(async move {
            let outgoing = outgoing
                .upgrade()
                .context("app-server current-time provider is unavailable")?;
            let started_at =
                request_current_time(outgoing.clone(), thread_state_manager.clone(), thread_id)
                    .await?;
            let wake_at = started_at
                .checked_add_signed(
                    chrono::Duration::from_std(duration)
                        .context("external sleep duration is outside the supported range")?,
                )
                .context("external sleep deadline is outside the supported range")?;

            let mut current = started_at;
            loop {
                if current >= wake_at {
                    return Ok(());
                }
                let remaining = (wake_at - current)
                    .to_std()
                    .context("external sleep remaining duration is outside the supported range")?;
                tokio::time::sleep(remaining.min(CURRENT_TIME_POLL_INTERVAL)).await;
                current =
                    request_current_time(outgoing.clone(), thread_state_manager.clone(), thread_id)
                        .await?;
            }
        })
    }
}

struct PendingCurrentTimeRequest {
    outgoing: Arc<OutgoingMessageSender>,
    request_id: Option<RequestId>,
}

impl PendingCurrentTimeRequest {
    fn disarm(&mut self) {
        self.request_id = None;
    }

    fn cancel(&mut self) {
        if let Some(request_id) = self.request_id.take() {
            let _canceled = self.outgoing.cancel_request_sync(&request_id);
        }
    }
}

impl Drop for PendingCurrentTimeRequest {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn request_current_time(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
    thread_id: ThreadId,
) -> Result<DateTime<Utc>> {
    let deadline = Instant::now() + CURRENT_TIME_REQUEST_TIMEOUT;
    timeout_at(
        deadline,
        thread_state_manager.wait_for_thread_subscriber(thread_id),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "timed out waiting for a client to subscribe to the thread after {}s",
            CURRENT_TIME_REQUEST_TIMEOUT.as_secs()
        )
    })?;
    let (request_id, rx) = timeout_at(deadline, async {
        let connection_ids = thread_state_manager
            .subscribed_connection_ids(thread_id)
            .await;
        let connection_id = require_single_current_time_connection(&connection_ids)?;
        // The subscription wait consumes the same budget as delivery and the
        // response. Do not publish a request once that budget has expired.
        if Instant::now() >= deadline {
            bail!("current-time request deadline expired before delivery");
        }
        let connection_ids = [connection_id];
        Ok(outgoing
            .send_request_to_connections(
                Some(&connection_ids),
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                Some(thread_id),
            )
            .await)
    })
    .await
    .map_err(|_| {
        anyhow!(
            "current-time request delivery timed out after {}s",
            CURRENT_TIME_REQUEST_TIMEOUT.as_secs()
        )
    })??;
    let mut pending_request = PendingCurrentTimeRequest {
        outgoing: outgoing.clone(),
        request_id: Some(request_id.clone()),
    };

    let response = timeout_at(deadline, rx).await;
    if response.is_ok() {
        pending_request.disarm();
    }
    let result = match response {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(err))) => {
            bail!(
                "current-time request failed: code={} message={}",
                err.code,
                err.message
            );
        }
        Ok(Err(err)) => bail!("current-time request was canceled: {err}"),
        Err(_) => {
            pending_request.cancel();
            bail!(
                "current-time request timed out after {}s",
                CURRENT_TIME_REQUEST_TIMEOUT.as_secs()
            );
        }
    };
    let response: CurrentTimeReadResponse =
        serde_json::from_value(result).context("invalid current-time response")?;

    DateTime::from_timestamp(response.current_time_at, 0)
        .ok_or_else(|| anyhow!("current-time response is outside the supported range"))
}

fn require_single_current_time_connection(connection_ids: &[ConnectionId]) -> Result<ConnectionId> {
    // External clocks are not interchangeable, so do not choose one silently.
    match connection_ids {
        [connection_id] => Ok(*connection_id),
        _ => bail!(
            "expected exactly one client subscribed to the thread, found {}",
            connection_ids.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use codex_protocol::ThreadId;
    use tokio::sync::mpsc;
    use tokio::time::Duration;
    use tokio::time::timeout;

    use super::request_current_time;
    use super::require_single_current_time_connection;
    use crate::outgoing_message::ConnectionId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessageSender;
    use crate::thread_state::ConnectionCapabilities;
    use crate::thread_state::ThreadStateManager;

    #[test]
    fn current_time_connection_must_be_unambiguous() {
        assert_eq!(
            require_single_current_time_connection(&[ConnectionId(7)]).unwrap(),
            ConnectionId(7)
        );
        assert_eq!(
            require_single_current_time_connection(&[])
                .unwrap_err()
                .to_string(),
            "expected exactly one client subscribed to the thread, found 0"
        );
        assert_eq!(
            require_single_current_time_connection(&[ConnectionId(7), ConnectionId(8)])
                .unwrap_err()
                .to_string(),
            "expected exactly one client subscribed to the thread, found 2"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn current_time_delivery_uses_remaining_subscription_budget() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(1);
        // Hold the transport's only slot so request delivery experiences real
        // channel backpressure without a consumer draining it.
        let capacity = outgoing_tx.clone().reserve_owned().await.unwrap();
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        outgoing
            .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
            .await;
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let provider =
            super::app_server_time_provider(outgoing.clone(), thread_state_manager.clone());
        let time_read = tokio::spawn(async move { provider.current_time(thread_id).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(8)).await;
        assert!(
            thread_state_manager
                .try_add_connection_to_thread(thread_id, connection_id)
                .await
        );
        timeout(Duration::from_secs(1), async {
            while outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delivery should register its callback before waiting for capacity");

        tokio::time::advance(Duration::from_secs(2)).await;
        let error = timeout(Duration::from_secs(1), time_read)
            .await
            .expect("delivery must time out using the original ten-second budget")
            .expect("current-time task should not panic")
            .expect_err("a full transport queue must time out");
        assert_eq!(
            error.to_string(),
            "current-time request delivery timed out after 10s"
        );
        timeout(Duration::from_secs(1), async {
            while !outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceled delivery must remove its callback");
        drop(capacity);
        assert!(matches!(
            outgoing_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn thread_cleanup_cancels_pending_external_current_time_request() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        outgoing
            .connection_opened(connection_id, Arc::new(AtomicBool::new(/*value*/ true)))
            .await;
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        assert!(
            thread_state_manager
                .try_add_connection_to_thread(thread_id, connection_id)
                .await
        );

        let time_read = tokio::spawn(request_current_time(
            outgoing.clone(),
            thread_state_manager,
            thread_id,
        ));
        timeout(Duration::from_secs(1), outgoing_rx.recv())
            .await
            .expect("current-time request should be sent before timeout")
            .expect("current-time request channel should remain open");
        assert_eq!(
            outgoing.pending_requests_for_thread(thread_id).await.len(),
            1
        );

        outgoing.cancel_requests_for_thread(thread_id, None).await;

        assert!(
            outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty()
        );
        let error = timeout(Duration::from_secs(1), time_read)
            .await
            .expect("current-time task should finish after thread cleanup")
            .expect("current-time task should not panic")
            .expect_err("thread cleanup should cancel the current-time request");
        assert!(
            error
                .to_string()
                .contains("current-time request was canceled")
        );
    }

    #[test]
    fn delivered_current_time_cancellation_removes_callback_before_runtime_shutdown() {
        use crate::outgoing_message::OutgoingMessage;
        use std::task::Poll;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("originating runtime");
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let connection = ConnectionId(71);
        runtime.block_on(async {
            outgoing
                .connection_opened(connection, Arc::new(AtomicBool::new(true)))
                .await;
            manager
                .connection_initialized(connection, ConnectionCapabilities::default())
                .await;
            assert!(
                manager
                    .try_add_connection_to_thread(thread_id, connection)
                    .await
            );
        });
        let provider = super::app_server_time_provider(Arc::clone(&outgoing), manager);
        let mut original = provider.current_time(thread_id);
        runtime.block_on(std::future::poll_fn(|cx| {
            assert!(original.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }));
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::Request(request),
            ..
        } = outgoing_rx
            .try_recv()
            .expect("normal current-time request actually delivered")
        else {
            panic!("expected current-time request");
        };
        assert_eq!(connection_id, connection);
        assert!(matches!(
            request,
            codex_app_server_protocol::ServerRequest::CurrentTimeRead { .. }
        ));
        let original_id = request.id().clone();
        assert_eq!(runtime.block_on(outgoing.pending_callback_count()), 1);
        {
            // Drop while a runtime is present but no executor is polling tasks.
            // Immediately destroying it must not strand scheduled cleanup.
            let _entered = runtime.enter();
            drop(original);
        }
        runtime.shutdown_timeout(Duration::from_millis(100));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("reconnect runtime");
        runtime.block_on(async {
            assert_eq!(
                outgoing.pending_callback_count().await,
                0,
                "callback table must be physically empty before disconnect cleanup"
            );
            assert!(
                outgoing
                    .pending_requests_for_thread(thread_id)
                    .await
                    .is_empty()
            );
            let reconnected = ConnectionId(72);
            outgoing
                .connection_opened(reconnected, Arc::new(AtomicBool::new(true)))
                .await;
            outgoing
                .replay_requests_to_connection_for_thread(reconnected, thread_id, true)
                .await;
            assert!(
                matches!(
                    outgoing_rx.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ),
                "cancelled current-time request replayed"
            );

            let mut fresh = provider.current_time(thread_id);
            std::future::poll_fn(|cx| {
                assert!(fresh.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::Request(request),
                ..
            } = outgoing_rx.try_recv().expect("fresh request delivered")
            else {
                panic!("expected fresh current-time request");
            };
            let fresh_id = request.id().clone();
            assert_ne!(fresh_id, original_id);
            outgoing
                .notify_client_response(
                    connection,
                    original_id,
                    serde_json::json!({"currentTimeAt": 1_600_000_000}),
                )
                .await;
            std::future::poll_fn(|cx| {
                assert!(
                    fresh.as_mut().poll(cx).is_pending(),
                    "late original response completed a fresh request"
                );
                Poll::Ready(())
            })
            .await;
            assert_eq!(outgoing.pending_callback_count().await, 1);
            outgoing
                .notify_client_response(
                    connection,
                    fresh_id,
                    serde_json::json!({"currentTimeAt": 1_700_000_000}),
                )
                .await;
            assert_eq!(
                fresh
                    .await
                    .expect("fresh current-time response")
                    .timestamp(),
                1_700_000_000
            );
            assert_eq!(outgoing.pending_callback_count().await, 0);
        });
    }
}
