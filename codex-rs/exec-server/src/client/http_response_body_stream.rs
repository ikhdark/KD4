//! Shared HTTP response-body stream plumbing for local and remote execution.
//!
//! This module owns the byte-stream type exposed by the `HttpClient`
//! capability plus the remote-side routing table used to turn
//! `http/request/bodyDelta` notifications back into per-request streams.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use codex_http_client::HttpError;
use codex_http_client::HttpResponse;
use futures::StreamExt;
use serde_json::Value;
use serde_json::from_value;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::debug;

use crate::client::ExecServerError;
use crate::client::HttpBodyStreams;
use crate::client::Inner;
use crate::protocol::HTTP_REQUEST_BODY_DELTA_METHOD;
use crate::protocol::HttpRequestBodyDeltaNotification;
use crate::rpc::RpcNotificationSender;

pub(super) struct HttpBodyStreamRegistration {
    inner: Arc<Inner>,
    request_id: String,
    active: bool,
    runtime: Handle,
}

enum HttpResponseBodyStreamInner {
    Local {
        body: Pin<Box<dyn futures::Stream<Item = Result<Bytes, HttpError>> + Send>>,
    },
    Remote {
        inner: Arc<Inner>,
        request_id: String,
        next_seq: u64,
        rx: mpsc::Receiver<HttpRequestBodyDeltaNotification>,
        pending_terminal: Option<Result<Option<Vec<u8>>, String>>,
        closed: bool,
        runtime: Handle,
    },
}

/// Request-scoped stream of body chunks for an HTTP response.
///
/// The initial `http/request` call returns status and headers. This stream then
/// receives the ordered `http/request/bodyDelta` notifications for that request
/// id until EOF or a terminal error.
pub struct HttpResponseBodyStream {
    inner: HttpResponseBodyStreamInner,
}

impl HttpResponseBodyStream {
    pub(super) fn local(response: HttpResponse) -> Self {
        Self {
            inner: HttpResponseBodyStreamInner::Local {
                body: Box::pin(response.bytes_stream()),
            },
        }
    }

    pub(super) fn remote(
        inner: Arc<Inner>,
        request_id: String,
        rx: mpsc::Receiver<HttpRequestBodyDeltaNotification>,
    ) -> Self {
        Self {
            inner: HttpResponseBodyStreamInner::Remote {
                inner,
                request_id,
                next_seq: 1,
                rx,
                pending_terminal: None,
                closed: false,
                runtime: Handle::current(),
            },
        }
    }

    /// Receives the next response-body chunk.
    ///
    /// Returns `Ok(None)` at EOF and converts sequence gaps or stream-side
    /// stream errors into protocol errors.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>, ExecServerError> {
        match &mut self.inner {
            HttpResponseBodyStreamInner::Local { body } => match body.next().await {
                Some(chunk) => match chunk {
                    Ok(bytes) => Ok(Some(bytes.to_vec())),
                    Err(error) => Err(ExecServerError::HttpRequest(error.to_string())),
                },
                None => Ok(None),
            },
            HttpResponseBodyStreamInner::Remote {
                inner,
                request_id,
                next_seq,
                rx,
                pending_terminal,
                closed,
                ..
            } => {
                if *closed {
                    return Ok(None);
                }
                if pending_terminal.is_none() {
                    let delta = match rx.recv().await {
                        Some(delta) => Ok(delta),
                        None => inner
                            .take_http_body_stream_terminal(request_id)
                            .await
                            .unwrap_or_else(|| Err("body channel closed before EOF".to_string())),
                    };
                    let outcome = match delta {
                        Ok(delta) if delta.seq != *next_seq => Err(format!(
                            "http response stream `{request_id}` received seq {}, expected {}",
                            delta.seq, *next_seq
                        )),
                        Ok(delta) => {
                            *next_seq += 1;
                            if let Some(error) = delta.error {
                                Err(format!(
                                    "http response stream `{request_id}` failed: {error}"
                                ))
                            } else {
                                let chunk = delta.delta.into_inner();
                                if !delta.done {
                                    return Ok(Some(chunk));
                                }
                                Ok((!chunk.is_empty()).then_some(chunk))
                            }
                        }
                        Err(error) => Err(format!(
                            "http response stream `{request_id}` failed: {error}"
                        )),
                    };
                    // Retain the consumed outcome before awaiting cleanup so a cancelled
                    // recv can be retried without losing the final chunk or error.
                    *pending_terminal = Some(outcome);
                }
                inner.abandon_http_body_stream(request_id).await;
                *closed = true;
                pending_terminal
                    .take()
                    .ok_or_else(|| {
                        ExecServerError::Protocol(
                            "HTTP body terminal outcome is missing".to_string(),
                        )
                    })?
                    .map_err(ExecServerError::Protocol)
            }
        }
    }
}

impl Drop for HttpResponseBodyStream {
    /// Schedules stream-route removal if the consumer drops before EOF.
    fn drop(&mut self) {
        if let HttpResponseBodyStreamInner::Remote {
            inner,
            request_id,
            closed,
            runtime,
            ..
        } = &mut self.inner
        {
            if *closed {
                return;
            }
            *closed = true;
            spawn_remove_http_body_stream(runtime, Arc::clone(inner), request_id.clone());
        }
    }
}

impl HttpBodyStreamRegistration {
    pub(super) fn new(inner: Arc<Inner>, request_id: String) -> Self {
        Self {
            inner,
            request_id,
            active: true,
            runtime: Handle::current(),
        }
    }

    pub(super) fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for HttpBodyStreamRegistration {
    /// Removes the route if the stream request future is cancelled before headers return.
    fn drop(&mut self) {
        if self.active {
            spawn_remove_http_body_stream(
                &self.runtime,
                Arc::clone(&self.inner),
                self.request_id.clone(),
            );
        }
    }
}

/// Schedules HTTP body route removal from synchronous drop paths.
fn spawn_remove_http_body_stream(runtime: &Handle, inner: Arc<Inner>, request_id: String) {
    runtime.spawn(async move {
        inner.abandon_http_body_stream(&request_id).await;
    });
}

pub(super) async fn send_body_delta(
    notifications: &RpcNotificationSender,
    delta: HttpRequestBodyDeltaNotification,
) -> bool {
    notifications
        .notify(HTTP_REQUEST_BODY_DELTA_METHOD, &delta)
        .await
        .is_ok()
}

impl Inner {
    /// Routes one streamed HTTP body notification into its request-local receiver.
    pub(crate) async fn handle_http_body_delta_notification(
        &self,
        params: Option<Value>,
    ) -> Result<(), ExecServerError> {
        let params: HttpRequestBodyDeltaNotification = from_value(params.unwrap_or(Value::Null))?;
        // Unknown request ids are ignored intentionally: a stream may have already
        // reached EOF and released its route.
        let request_id = params.request_id.clone();
        let mut state = self.http_body_streams.lock().await;
        if let Some(tx) = state.streams.get(&request_id) {
            if params.done || params.error.is_some() {
                if !tx.is_closed() {
                    // Publish before closing data admission, independently of queue capacity.
                    state.terminals.insert(request_id.clone(), Ok(params));
                }
                state.streams.remove(&request_id);
            } else {
                match tx.try_send(params) {
                    Ok(()) => {}
                    Err(TrySendError::Closed(_)) => {
                        state.streams.remove(&request_id);
                    }
                    Err(TrySendError::Full(_)) => {
                        state.terminals.insert(
                            request_id.clone(),
                            Err("body delta channel filled before delivery".into()),
                        );
                        state.streams.remove(&request_id);
                        self.cancel_remote_http_body(&request_id);
                        debug!(
                            "closing http response stream `{request_id}` after body delta backpressure"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Fails active streamed HTTP bodies so callers do not wait forever after a
    /// transport disconnect or notification handling failure.
    pub(crate) async fn fail_all_http_body_streams(&self, message: String) {
        let mut state = self.http_body_streams.lock().await;
        let HttpBodyStreams { streams, terminals } = &mut *state;
        for (request_id, tx) in streams.drain() {
            if !tx.is_closed() {
                terminals.insert(request_id, Err(message.clone()));
            }
        }
    }

    /// Allocates a connection-local streamed HTTP response id.
    pub(super) fn next_http_body_stream_request_id(&self) -> String {
        let id = self
            .http_body_stream_next_id
            .fetch_add(1, Ordering::Relaxed);
        format!("http-{id}")
    }

    /// Registers a request id before issuing a streaming HTTP call.
    pub(super) async fn insert_http_body_stream(
        &self,
        request_id: String,
        tx: mpsc::Sender<HttpRequestBodyDeltaNotification>,
    ) -> Result<(), ExecServerError> {
        let mut state = self.http_body_streams.lock().await;
        if state.streams.contains_key(&request_id) {
            return Err(ExecServerError::Protocol(format!(
                "http response stream already registered for request {request_id}"
            )));
        }
        state.terminals.remove(&request_id);
        state.streams.insert(request_id, tx);
        Ok(())
    }

    /// Removes a request id after EOF, terminal error, or request failure.
    #[cfg(test)]
    pub(super) async fn remove_http_body_stream(
        &self,
        request_id: &str,
    ) -> Option<mpsc::Sender<HttpRequestBodyDeltaNotification>> {
        self.http_body_streams
            .lock()
            .await
            .streams
            .remove(request_id)
    }

    #[cfg(test)]
    async fn record_http_body_stream_failure(&self, request_id: &str, message: String) {
        let mut state = self.http_body_streams.lock().await;
        // An abandonment may already have removed this route while delivery was in flight.
        let Some(tx) = state.streams.remove(request_id) else {
            return;
        };
        if !tx.is_closed() {
            state.terminals.insert(request_id.to_string(), Err(message));
        }
    }

    /// Consumer abandonment clears both routing and any undelivered failure atomically.
    pub(super) async fn abandon_http_body_stream(&self, request_id: &str) {
        let active = {
            let mut state = self.http_body_streams.lock().await;
            let active = state.streams.remove(request_id).is_some();
            state.terminals.remove(request_id);
            active
        };
        if active {
            self.cancel_remote_http_body(request_id);
        }
    }

    fn cancel_remote_http_body(&self, request_id: &str) {
        let rpc_client = {
            let state = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &state.status {
                crate::client::ConnectionStatus::Connected(client) => Arc::clone(client),
                _ => return,
            }
        };
        let params = crate::protocol::HttpRequestCancelParams {
            request_id: request_id.to_string(),
        };
        // A request, rather than an unknown notification, keeps older peers compatible.
        // Do not recover or replay a connection just to cancel obsolete work.
        tokio::spawn(async move {
            let _: Result<(), _> = rpc_client
                .call_for_cleanup(
                    crate::protocol::HTTP_REQUEST_CANCEL_METHOD,
                    &params,
                    std::time::Duration::from_secs(5),
                )
                .await;
        });
    }

    async fn take_http_body_stream_terminal(
        &self,
        request_id: &str,
    ) -> Option<Result<HttpRequestBodyDeltaNotification, String>> {
        self.http_body_streams
            .lock()
            .await
            .terminals
            .remove(request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ConnectionState;
    use crate::client::ConnectionStatus;
    use crate::client::HttpBodyStreams;
    use arc_swap::ArcSwap;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicU64;
    use tokio::sync::Mutex;
    use tokio::sync::watch;

    fn inner() -> Arc<Inner> {
        Arc::new(Inner {
            file_read_slots: Arc::new(tokio::sync::Semaphore::new(
                crate::file_read::MAX_OPEN_FILE_READS,
            )),
            connection: StdMutex::new(ConnectionState {
                status: ConnectionStatus::Failed("test has no transport".into()),
                active_process_starts: 0,
            }),
            connection_changed: watch::channel(()).0,
            sessions: ArcSwap::from_pointee(HashMap::new()),
            sessions_write_lock: StdMutex::new(()),
            http_body_streams: Mutex::new(HttpBodyStreams::default()),
            http_body_stream_next_id: AtomicU64::new(1),
            session_id: OnceLock::new(),
            environment_info: OnceLock::new(),
            reconnect_strategy: None,
        })
    }

    async fn register(inner: &Arc<Inner>) -> HttpResponseBodyStream {
        let (tx, rx) = mpsc::channel(1);
        inner
            .insert_http_body_stream("test".into(), tx)
            .await
            .expect("register");
        HttpResponseBodyStream::remote(Arc::clone(inner), "test".into(), rx)
    }

    fn delta(seq: u64, done: bool, error: Option<&str>) -> Option<Value> {
        Some(
            serde_json::to_value(HttpRequestBodyDeltaNotification {
                request_id: "test".into(),
                seq,
                delta: b"last".to_vec().into(),
                done,
                error: error.map(str::to_string),
            })
            .expect("serialize notification"),
        )
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Deliberately blocks terminal cleanup to cancel its first poll and verify retry preserves the outcome"
    )]
    async fn terminal_outcome_survives_cancelled_cleanup() {
        for (seq, error, expected_error) in [
            (1, None, None),
            (1, Some("remote failure"), Some("remote failure")),
            (2, None, Some("received seq 2, expected 1")),
        ] {
            let inner = inner();
            let mut stream = register(&inner).await;
            inner
                .handle_http_body_delta_notification(delta(seq, true, error))
                .await
                .expect("route delta");
            let guard = inner.http_body_streams.lock().await;
            {
                let mut receive = Box::pin(stream.recv());
                assert!(futures::poll!(receive.as_mut()).is_pending());
            }
            drop(guard);
            let outcome = stream.recv().await;
            if let Some(expected) = expected_error {
                assert!(
                    outcome
                        .expect_err("terminal error")
                        .to_string()
                        .contains(expected)
                );
            } else {
                assert_eq!(outcome.expect("final data"), Some(b"last".to_vec()));
            }
            assert_eq!(stream.recv().await.expect("closed stream"), None);
            assert!(inner.http_body_streams.lock().await.streams.is_empty());
            assert!(inner.http_body_streams.lock().await.terminals.is_empty());
        }
    }

    #[test]
    fn dropping_overflowed_stream_outside_runtime_clears_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let inner = inner();
        let stream = runtime.block_on(async {
            let stream = register(&inner).await;
            inner
                .handle_http_body_delta_notification(delta(1, false, None))
                .await
                .expect("first delta");
            inner
                .handle_http_body_delta_notification(delta(2, false, None))
                .await
                .expect("overflow delta");
            assert_eq!(
                inner
                    .http_body_streams
                    .lock()
                    .await
                    .terminals
                    .get("test")
                    .and_then(|terminal| terminal.as_ref().err().map(String::as_str)),
                Some("body delta channel filled before delivery")
            );
            stream
        });
        drop(stream);
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while !inner.http_body_streams.lock().await.terminals.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("drop cleanup must run on owning runtime");
            inner
                .record_http_body_stream_failure("test", "late failure".into())
                .await;
            assert!(inner.http_body_streams.lock().await.terminals.is_empty());
            assert!(inner.http_body_streams.lock().await.streams.is_empty());
        });
    }

    #[tokio::test]
    async fn disconnect_preserves_each_live_stream_failure_and_clears_routes() {
        let inner = inner();
        let mut first = register(&inner).await;
        let (tx, rx) = mpsc::channel(1);
        inner
            .insert_http_body_stream("second".into(), tx)
            .await
            .expect("register second");
        let mut second = HttpResponseBodyStream::remote(Arc::clone(&inner), "second".into(), rx);
        let (duplicate, _) = mpsc::channel(1);
        assert!(
            inner
                .insert_http_body_stream("test".into(), duplicate)
                .await
                .is_err()
        );
        inner
            .fail_all_http_body_streams("disconnected".into())
            .await;
        assert!(inner.http_body_streams.lock().await.streams.is_empty());
        for stream in [&mut first, &mut second] {
            assert!(
                stream
                    .recv()
                    .await
                    .expect_err("disconnect failure")
                    .to_string()
                    .contains("disconnected")
            );
            assert_eq!(stream.recv().await.expect("closed"), None);
        }
        assert!(inner.http_body_streams.lock().await.terminals.is_empty());
    }

    #[tokio::test]
    async fn channel_close_without_terminal_frame_is_an_error() {
        let inner = inner();
        let mut stream = register(&inner).await;
        inner.remove_http_body_stream("test").await;
        assert!(
            stream
                .recv()
                .await
                .expect_err("premature close")
                .to_string()
                .contains("body channel closed before EOF")
        );
        assert_eq!(stream.recv().await.expect("closed"), None);
    }
    #[tokio::test]
    async fn full_data_queue_preserves_eof_error_and_final_bytes() {
        for (error, bytes, seq) in [
            (None, vec![], 257),
            (None, b"final".to_vec(), 257),
            (Some("remote error"), vec![], 257),
            (None, vec![], 258),
        ] {
            let inner = inner();
            let (tx, rx) = mpsc::channel(256);
            inner
                .insert_http_body_stream("test".into(), tx)
                .await
                .unwrap();
            let mut stream = HttpResponseBodyStream::remote(inner.clone(), "test".into(), rx);
            for seq in 1..=256 {
                inner
                    .handle_http_body_delta_notification(delta(seq, false, None))
                    .await
                    .unwrap();
            }
            inner
                .handle_http_body_delta_notification(Some(
                    serde_json::to_value(HttpRequestBodyDeltaNotification {
                        request_id: "test".into(),
                        seq,
                        delta: bytes.clone().into(),
                        done: true,
                        error: error.map(str::to_string),
                    })
                    .unwrap(),
                ))
                .await
                .unwrap();
            inner
                .fail_all_http_body_streams("later disconnect".into())
                .await;
            for _ in 1..=256 {
                assert_eq!(stream.recv().await.unwrap(), Some(b"last".to_vec()));
            }
            let terminal = stream.recv().await;
            if seq != 257 {
                assert!(terminal.unwrap_err().to_string().contains("expected 257"));
            } else if let Some(error) = error {
                assert!(terminal.unwrap_err().to_string().contains(error));
            } else {
                assert_eq!(terminal.unwrap(), (!bytes.is_empty()).then_some(bytes));
            }
            assert_eq!(stream.recv().await.unwrap(), None);
            assert!(inner.http_body_streams.lock().await.terminals.is_empty());
        }
    }
}
