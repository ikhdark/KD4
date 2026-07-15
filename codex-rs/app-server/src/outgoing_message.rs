use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_analytics::AnalyticsEventsClient;
use codex_analytics::TurnDeliveryFact;
use codex_analytics::TurnDeliveryStatus;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::ExperimentalApi;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestPayload;
use codex_app_server_protocol::ServerResponse;
use codex_otel::span_w3c_trace_context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument;
use tracing::Span;
use tracing::warn;

use crate::error_code::internal_error;
use crate::server_request_error::TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON;
use crate::thread_state::RecipientSnapshot;
pub(crate) use codex_app_server_transport::ConnectionId;
pub(crate) use codex_app_server_transport::OutgoingError;
pub(crate) use codex_app_server_transport::OutgoingMessage;
pub(crate) use codex_app_server_transport::OutgoingResponse;
pub(crate) use codex_app_server_transport::QueuedOutgoingMessage;

#[cfg(test)]
use codex_protocol::account::PlanType;

pub(crate) type ClientRequestResult = std::result::Result<Result, JSONRPCErrorError>;

const TURN_DELIVERY_RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_DELIVERY_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Stable identifier for a client request scoped to a transport connection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionRequestId {
    pub(crate) connection_id: ConnectionId,
    pub(crate) request_id: RequestId,
}

/// Trace data we keep for an incoming request until we send its final
/// response or error.
#[derive(Clone)]
pub(crate) struct RequestContext {
    request_id: ConnectionRequestId,
    span: Span,
    parent_trace: Option<W3cTraceContext>,
}

impl RequestContext {
    pub(crate) fn new(
        request_id: ConnectionRequestId,
        span: Span,
        parent_trace: Option<W3cTraceContext>,
    ) -> Self {
        Self {
            request_id,
            span,
            parent_trace,
        }
    }

    pub(crate) fn request_trace(&self) -> Option<W3cTraceContext> {
        span_w3c_trace_context(&self.span).or_else(|| self.parent_trace.clone())
    }

    pub(crate) fn span(&self) -> Span {
        self.span.clone()
    }

    fn record_turn_id(&self, turn_id: &str) {
        self.span.record("turn.id", turn_id);
    }
}

#[derive(Debug)]
pub(crate) enum OutgoingEnvelope {
    ToConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
        write_complete_tx: Option<oneshot::Sender<()>>,
    },
    ToSnapshotAcceptedConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
        write_complete_tx: Option<oneshot::Sender<()>>,
    },
    Broadcast {
        message: OutgoingMessage,
    },
}

struct CloneForAllButLast<T> {
    value: Option<T>,
    remaining: usize,
}

impl<T> CloneForAllButLast<T> {
    fn new(value: T, recipients: usize) -> Self {
        Self {
            value: Some(value),
            remaining: recipients,
        }
    }
}

impl<T: Clone> Iterator for CloneForAllButLast<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        if self.remaining == 0 {
            self.value.take()
        } else {
            Some(
                self.value
                    .as_ref()
                    .expect("fanout value remains available before the final recipient")
                    .clone(),
            )
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<T: Clone> ExactSizeIterator for CloneForAllButLast<T> {}

/// Sends messages to the client and manages request callbacks.
pub(crate) struct OutgoingMessageSender {
    next_server_request_id: AtomicI64,
    sender: mpsc::Sender<OutgoingEnvelope>,
    request_id_to_callback: Mutex<HashMap<RequestId, PendingCallbackEntry>>,
    /// Incoming requests that are still waiting on a final response or error.
    /// We keep them here because this is where responses, errors, and
    /// disconnect cleanup all get handled.
    request_contexts: Mutex<HashMap<ConnectionRequestId, RequestContext>>,
    analytics_events_client: AnalyticsEventsClient,
    delivery_tasks: TaskTracker,
    delivery_shutdown: CancellationToken,
    delivery_accepting: Mutex<bool>,
}

#[derive(Clone)]
pub(crate) struct ThreadScopedOutgoingMessageSender {
    outgoing: Arc<OutgoingMessageSender>,
    recipients: RecipientSnapshot,
    thread_id: ThreadId,
}

struct PendingCallbackEntry {
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
    request: ServerRequest,
}

struct PendingTurnDeliveryReceipt {
    connection_id: ConnectionId,
    receiver: Option<oneshot::Receiver<()>>,
    immediate_outcome: Option<TurnDeliveryOutcomeKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnDeliveryOutcomeKind {
    Success,
    Failure,
    Timeout,
    ShutdownCancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TurnDeliveryOutcome {
    connection_id: ConnectionId,
    kind: TurnDeliveryOutcomeKind,
    successful_elapsed_ms: Option<u64>,
    post_core_delivery_latency_ms: Option<u64>,
}

impl ThreadScopedOutgoingMessageSender {
    /// Dispatches a terminal turn notification to one frozen target set and
    /// collects transport writer receipts without delaying core completion.
    pub(crate) async fn send_server_notification_with_receipts(
        &self,
        notification: ServerNotification,
        origin_connection_id: Option<ConnectionId>,
    ) {
        self.outgoing
            .analytics_events_client
            .track_notification(&notification);

        let target_connection_ids = self.accepted_connection_ids(&notification);
        let ServerNotification::TurnCompleted(completed) = &notification else {
            if !target_connection_ids.is_empty() {
                self.outgoing
                    .send_snapshot_notification_to_connections(
                        &target_connection_ids,
                        notification,
                    )
                    .await;
            }
            return;
        };
        let turn_id = completed.turn.id.clone();
        let core_completed_at_ms = completed
            .timing
            .as_ref()
            .and_then(|timing| timing.completed_at_unix_ms)
            .or_else(|| {
                completed
                    .turn
                    .completed_at
                    .and_then(|seconds| seconds.checked_mul(1_000))
            })
            .and_then(|milliseconds| u64::try_from(milliseconds).ok());

        let mut target_connection_ids = target_connection_ids;
        target_connection_ids.sort_unstable_by_key(|connection_id| connection_id.0);
        target_connection_ids.dedup();
        self.outgoing
            .dispatch_turn_completed_with_receipts(
                self.thread_id,
                turn_id,
                target_connection_ids,
                origin_connection_id,
                core_completed_at_ms,
                notification,
            )
            .await;
    }

    #[cfg(test)]
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        connection_ids: Vec<ConnectionId>,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            outgoing,
            recipients: RecipientSnapshot::permissive(connection_ids),
            thread_id,
        }
    }

    pub(crate) fn from_snapshot(
        outgoing: Arc<OutgoingMessageSender>,
        recipients: RecipientSnapshot,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            outgoing,
            recipients,
            thread_id,
        }
    }

    fn accepted_connection_ids(&self, notification: &ServerNotification) -> Vec<ConnectionId> {
        let is_raw = matches!(notification, ServerNotification::RawResponseItemCompleted(_));
        let is_experimental = notification.experimental_reason().is_some();
        let method = notification.to_string();
        self.recipients
            .descriptors()
            .iter()
            .filter(|descriptor| !is_raw || descriptor.raw_events_enabled)
            .filter(|descriptor| {
                !is_experimental || descriptor.delivery_filter.experimental_api_enabled
            })
            .filter(|descriptor| {
                !descriptor
                    .delivery_filter
                    .opted_out_notification_methods
                    .contains(method.as_str())
            })
            .map(|descriptor| descriptor.connection_id)
            .collect()
    }

    pub(crate) async fn send_request(
        &self,
        payload: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.outgoing
            .send_request_to_connections(
                Some(self.recipients.connection_ids()),
                payload,
                Some(self.thread_id),
            )
            .await
    }

    pub(crate) fn track_effective_permissions_approval_response(
        &self,
        request_id: RequestId,
        response: RequestPermissionsResponse,
    ) {
        self.outgoing
            .analytics_events_client
            .track_effective_permissions_approval_response(
                now_unix_timestamp_ms(),
                request_id,
                response,
            );
    }

    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        self.outgoing
            .analytics_events_client
            .track_notification(&notification);
        let connection_ids = self.accepted_connection_ids(&notification);
        if connection_ids.is_empty() {
            return;
        }
        self.outgoing
            .send_snapshot_notification_to_connections(&connection_ids, notification)
            .await;
    }

    pub(crate) async fn send_global_server_notification(&self, notification: ServerNotification) {
        self.outgoing.send_server_notification(notification).await;
    }

    pub(crate) async fn abort_pending_server_requests(&self) {
        self.outgoing
            .cancel_requests_for_thread(
                self.thread_id,
                Some({
                    let mut error = internal_error(
                        "client request resolved because the turn state was changed",
                    );
                    error.data = Some(serde_json::json!({
                        "reason": TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON,
                    }));
                    error
                }),
            )
            .await
    }

    pub(crate) async fn send_response<T>(&self, request_id: ConnectionRequestId, response: T)
    where
        T: Into<ClientResponsePayload>,
    {
        self.outgoing.send_response(request_id, response).await;
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: impl Into<JSONRPCErrorError>,
    ) {
        self.outgoing.send_error(request_id, error).await;
    }
}

impl OutgoingMessageSender {
    pub(crate) fn new(
        sender: mpsc::Sender<OutgoingEnvelope>,
        analytics_events_client: AnalyticsEventsClient,
    ) -> Self {
        Self {
            next_server_request_id: AtomicI64::new(0),
            sender,
            request_id_to_callback: Mutex::new(HashMap::new()),
            request_contexts: Mutex::new(HashMap::new()),
            analytics_events_client,
            delivery_tasks: TaskTracker::new(),
            delivery_shutdown: CancellationToken::new(),
            delivery_accepting: Mutex::new(true),
        }
    }

    pub(crate) async fn register_request_context(&self, request_context: RequestContext) {
        let mut request_contexts = self.request_contexts.lock().await;
        if request_contexts
            .insert(request_context.request_id.clone(), request_context)
            .is_some()
        {
            warn!("replaced unresolved request context");
        }
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let mut request_contexts = self.request_contexts.lock().await;
        request_contexts.retain(|request_id, _| request_id.connection_id != connection_id);
    }

    pub(crate) async fn request_trace_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<W3cTraceContext> {
        let request_contexts = self.request_contexts.lock().await;
        request_contexts
            .get(request_id)
            .and_then(RequestContext::request_trace)
    }

    pub(crate) async fn record_request_turn_id(
        &self,
        request_id: &ConnectionRequestId,
        turn_id: &str,
    ) {
        let request_contexts = self.request_contexts.lock().await;
        if let Some(request_context) = request_contexts.get(request_id) {
            request_context.record_turn_id(turn_id);
        }
    }

    async fn take_request_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<RequestContext> {
        let mut request_contexts = self.request_contexts.lock().await;
        request_contexts.remove(request_id)
    }

    #[cfg(test)]
    async fn request_context_count(&self) -> usize {
        self.request_contexts.lock().await.len()
    }

    pub(crate) async fn send_request(
        &self,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.send_request_to_connections(
            /*connection_ids*/ None, request, /*thread_id*/ None,
        )
        .await
    }

    fn next_request_id(&self) -> RequestId {
        RequestId::Integer(self.next_server_request_id.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) async fn send_request_to_connections(
        &self,
        connection_ids: Option<&[ConnectionId]>,
        request: ServerRequestPayload,
        thread_id: Option<ThreadId>,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        let id = self.next_request_id();
        let outgoing_message_id = id.clone();
        let request = request.request_with_id(outgoing_message_id.clone());

        let (tx_approve, rx_approve) = oneshot::channel();
        {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.insert(
                id,
                PendingCallbackEntry {
                    callback: tx_approve,
                    thread_id,
                    request: request.clone(),
                },
            );
        }

        let outgoing_message = OutgoingMessage::Request(request.clone());
        let send_result = match connection_ids {
            None => {
                self.sender
                    .send(OutgoingEnvelope::Broadcast {
                        message: outgoing_message,
                    })
                    .await
            }
            Some(connection_ids) => {
                let mut send_error = None;
                for connection_id in connection_ids {
                    if let Err(err) = self
                        .sender
                        .send(OutgoingEnvelope::ToConnection {
                            connection_id: *connection_id,
                            message: outgoing_message.clone(),
                            write_complete_tx: None,
                        })
                        .await
                    {
                        send_error = Some(err);
                        break;
                    } else {
                        self.analytics_events_client
                            .track_server_request(connection_id.0, request.clone());
                    }
                }
                match send_error {
                    Some(err) => Err(err),
                    None => Ok(()),
                }
            }
        };

        if let Err(err) = send_result {
            warn!("failed to send request {outgoing_message_id:?} to client: {err:?}");
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.remove(&outgoing_message_id);
        }
        (outgoing_message_id, rx_approve)
    }

    pub(crate) async fn replay_requests_to_connection_for_thread(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) {
        let requests = self.pending_requests_for_thread(thread_id).await;
        for request in requests {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request),
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to resend request to client: {err:?}");
            }
        }
    }

    pub(crate) async fn notify_client_response(&self, id: RequestId, result: Result) {
        let entry = self.take_request_callback(&id).await;

        match entry {
            Some((id, entry)) => {
                let completed_at_ms = now_unix_timestamp_ms();
                if let Ok(response) = entry.request.response_from_result(result.clone())
                    && !matches!(response, ServerResponse::PermissionsRequestApproval { .. })
                {
                    self.analytics_events_client
                        .track_server_response(completed_at_ms, response);
                }
                if let Err(err) = entry.callback.send(Ok(result)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn notify_client_error(&self, id: RequestId, error: JSONRPCErrorError) {
        let entry = self.take_request_callback(&id).await;

        match entry {
            Some((id, entry)) => {
                warn!("client responded with error for {id:?}: {error:?}");
                self.analytics_events_client
                    .track_server_request_aborted(now_unix_timestamp_ms(), id.clone());
                if let Err(err) = entry.callback.send(Err(error)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn cancel_request(&self, id: &RequestId) -> bool {
        let entry = self.take_request_callback(id).await;
        if let Some((request_id, _entry)) = entry {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), request_id);
            true
        } else {
            false
        }
    }

    pub(crate) async fn cancel_all_requests(&self, error: Option<JSONRPCErrorError>) {
        let entries = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };

        for entry in entries {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), entry.request.id().clone());
            if let Some(error) = error.as_ref()
                && let Err(err) = entry.callback.send(Err(error.clone()))
            {
                let request_id = entry.request.id();
                warn!("could not notify callback for {request_id:?} due to: {err:?}");
            }
        }
    }

    async fn take_request_callback(
        &self,
        id: &RequestId,
    ) -> Option<(RequestId, PendingCallbackEntry)> {
        let mut request_id_to_callback = self.request_id_to_callback.lock().await;
        request_id_to_callback.remove_entry(id)
    }

    pub(crate) async fn pending_requests_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ServerRequest> {
        let request_id_to_callback = self.request_id_to_callback.lock().await;
        let mut requests = request_id_to_callback
            .values()
            .filter_map(|entry| {
                (entry.thread_id == Some(thread_id)).then_some(entry.request.clone())
            })
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.id().cmp(right.id()));
        requests
    }

    pub(crate) async fn cancel_requests_for_thread(
        &self,
        thread_id: ThreadId,
        error: Option<JSONRPCErrorError>,
    ) {
        let entries = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            let request_ids = request_id_to_callback
                .iter()
                .filter_map(|(request_id, entry)| {
                    (entry.thread_id == Some(thread_id)).then_some(request_id.clone())
                })
                .collect::<Vec<_>>();

            let mut entries = Vec::with_capacity(request_ids.len());
            for request_id in request_ids {
                if let Some(entry) = request_id_to_callback.remove(&request_id) {
                    entries.push(entry);
                }
            }
            entries
        };

        for entry in entries {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), entry.request.id().clone());
            if let Some(error) = error.as_ref()
                && let Err(err) = entry.callback.send(Err(error.clone()))
            {
                let request_id = entry.request.id();
                warn!("could not notify callback for {request_id:?} due to: {err:?}",);
            }
        }
    }

    pub(crate) async fn send_response<T>(&self, request_id: ConnectionRequestId, response: T)
    where
        T: Into<ClientResponsePayload>,
    {
        self.send_response_as_inner(request_id, response.into(), /*thread_originator*/ None)
            .await;
    }

    pub(crate) async fn send_response_with_thread_originator<T>(
        &self,
        request_id: ConnectionRequestId,
        response: T,
        thread_originator: String,
    ) where
        T: Into<ClientResponsePayload>,
    {
        self.send_response_as_inner(request_id, response.into(), Some(thread_originator))
            .await;
    }

    pub(crate) async fn send_response_as(
        &self,
        request_id: ConnectionRequestId,
        response: ClientResponsePayload,
    ) {
        self.send_response_as_inner(request_id, response, /*thread_originator*/ None)
            .await;
    }

    async fn send_response_as_inner(
        &self,
        request_id: ConnectionRequestId,
        response: ClientResponsePayload,
        thread_originator: Option<String>,
    ) {
        let connection_id = request_id.connection_id;
        let request_id_for_analytics = request_id.request_id.clone();
        let serialized_response = response
            .into_jsonrpc_parts_and_payload(request_id.request_id.clone())
            .map(|(id, result, response)| {
                if let Some(response) = response {
                    match thread_originator {
                        Some(thread_originator) => {
                            self.analytics_events_client
                                .track_response_with_thread_originator(
                                    connection_id.0,
                                    request_id_for_analytics,
                                    response,
                                    thread_originator,
                                );
                        }
                        None => {
                            self.analytics_events_client.track_response(
                                connection_id.0,
                                request_id_for_analytics,
                                response,
                            );
                        }
                    }
                }
                (id, result)
            });
        let request_context = self.take_request_context(&request_id).await;

        match serialized_response {
            Ok((id, result)) => {
                let outgoing_message = OutgoingMessage::Response(OutgoingResponse { id, result });
                self.send_outgoing_message_to_connection(
                    request_context,
                    connection_id,
                    outgoing_message,
                    "response",
                )
                .await;
            }
            Err(err) => {
                self.send_error_inner(
                    request_context,
                    request_id,
                    internal_error(format!("failed to serialize response: {err}")),
                )
                .await;
            }
        }
    }

    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        self.send_server_notification_to_connections(&[], notification)
            .await;
    }

    pub(crate) async fn send_server_notification_to_connections(
        &self,
        connection_ids: &[ConnectionId],
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = connection_ids.len(),
            "app-server event: {notification}"
        );
        let outgoing_message = OutgoingMessage::AppServerNotification(notification.clone());
        if connection_ids.is_empty() {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::Broadcast {
                    message: outgoing_message,
                })
                .await
            {
                warn!("failed to send server notification to client: {err:?}");
            }
            return;
        }
        for connection_id in connection_ids {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id: *connection_id,
                    message: outgoing_message.clone(),
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to send server notification to client: {err:?}");
            }
        }
    }

    async fn send_snapshot_notification_to_connections(
        &self,
        connection_ids: &[ConnectionId],
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = connection_ids.len(),
            "app-server snapshot event: {notification}"
        );
        let outgoing_message = OutgoingMessage::AppServerNotification(notification);
        for (connection_id, message) in connection_ids.iter().copied().zip(
            CloneForAllButLast::new(outgoing_message, connection_ids.len()),
        ) {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToSnapshotAcceptedConnection {
                    connection_id,
                    message,
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to send server notification to client: {err:?}");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch_turn_completed_with_receipts(
        &self,
        thread_id: ThreadId,
        turn_id: String,
        target_connection_ids: Vec<ConnectionId>,
        origin_connection_id: Option<ConnectionId>,
        core_completed_at_ms: Option<u64>,
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = target_connection_ids.len(),
            "app-server terminal event: {notification}"
        );
        let dispatch_started = Instant::now();
        let receipt_deadline = tokio::time::Instant::now() + TURN_DELIVERY_RECEIPT_TIMEOUT;
        let post_core_dispatch_latency_ms = core_completed_at_ms
            .map(|completed_at_ms| now_unix_timestamp_ms().saturating_sub(completed_at_ms));
        let outgoing_message = OutgoingMessage::AppServerNotification(notification);
        let mut receipts = Vec::with_capacity(target_connection_ids.len());
        let target_count = target_connection_ids.len();

        for (connection_id, message) in target_connection_ids
            .into_iter()
            .zip(CloneForAllButLast::new(outgoing_message, target_count))
        {
            let (write_complete_tx, write_complete_rx) = oneshot::channel();
            let send_result = self
                .sender
                .send(OutgoingEnvelope::ToSnapshotAcceptedConnection {
                    connection_id,
                    message,
                    write_complete_tx: Some(write_complete_tx),
                })
                .await;
            let immediate_outcome = if let Err(err) = send_result {
                warn!("failed to dispatch terminal notification to {connection_id:?}: {err:?}");
                Some(TurnDeliveryOutcomeKind::Failure)
            } else {
                None
            };
            receipts.push(PendingTurnDeliveryReceipt {
                connection_id,
                receiver: immediate_outcome.is_none().then_some(write_complete_rx),
                immediate_outcome,
            });
        }

        if receipts.is_empty() {
            self.analytics_events_client
                .track_turn_delivery(aggregate_turn_delivery(
                    thread_id,
                    turn_id,
                    origin_connection_id,
                    Vec::new(),
                ));
            return;
        }

        let accepting = self.delivery_accepting.lock().await;
        if *accepting {
            let analytics_events_client = self.analytics_events_client.clone();
            let delivery_shutdown = self.delivery_shutdown.clone();
            self.delivery_tasks.spawn(async move {
                let outcomes = collect_turn_delivery_outcomes(
                    receipts,
                    dispatch_started,
                    receipt_deadline,
                    post_core_dispatch_latency_ms,
                    delivery_shutdown,
                )
                .await;
                analytics_events_client.track_turn_delivery(aggregate_turn_delivery(
                    thread_id,
                    turn_id,
                    origin_connection_id,
                    outcomes,
                ));
            });
            return;
        }
        drop(accepting);

        let outcomes = receipts
            .into_iter()
            .map(|receipt| TurnDeliveryOutcome {
                connection_id: receipt.connection_id,
                kind: receipt
                    .immediate_outcome
                    .unwrap_or(TurnDeliveryOutcomeKind::ShutdownCancelled),
                successful_elapsed_ms: None,
                post_core_delivery_latency_ms: None,
            })
            .collect();
        self.analytics_events_client
            .track_turn_delivery(aggregate_turn_delivery(
                thread_id,
                turn_id,
                origin_connection_id,
                outcomes,
            ));
    }

    pub(crate) async fn shutdown_delivery_tasks(&self) {
        {
            let mut accepting = self.delivery_accepting.lock().await;
            *accepting = false;
            self.delivery_tasks.close();
        }
        if tokio::time::timeout(TURN_DELIVERY_SHUTDOWN_GRACE, self.delivery_tasks.wait())
            .await
            .is_err()
        {
            self.delivery_shutdown.cancel();
            self.delivery_tasks.wait().await;
        }
    }

    pub(crate) async fn send_server_notification_to_connection_and_wait(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
    ) {
        tracing::trace!("app-server event: {notification}");
        let outgoing_message = OutgoingMessage::AppServerNotification(notification.clone());
        let (write_complete_tx, write_complete_rx) = oneshot::channel();
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: outgoing_message,
                write_complete_tx: Some(write_complete_tx),
            })
            .await
        {
            warn!("failed to send server notification to client: {err:?}");
        }
        let _ = write_complete_rx.await;
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: impl Into<JSONRPCErrorError>,
    ) {
        let request_context = self.take_request_context(&request_id).await;
        self.send_error_inner(request_context, request_id, error.into())
            .await;
    }

    pub(crate) async fn send_result<T, E>(
        &self,
        request_id: ConnectionRequestId,
        result: std::result::Result<T, E>,
    ) where
        T: Into<ClientResponsePayload>,
        E: Into<JSONRPCErrorError>,
    {
        match result {
            Ok(response) => {
                self.send_response(request_id, response).await;
            }
            Err(error) => self.send_error(request_id, error).await,
        }
    }

    async fn send_error_inner(
        &self,
        request_context: Option<RequestContext>,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        let outgoing_message = OutgoingMessage::Error(OutgoingError {
            id: request_id.request_id,
            error,
        });
        self.send_outgoing_message_to_connection(
            request_context,
            request_id.connection_id,
            outgoing_message,
            "error",
        )
        .await;
    }

    async fn send_outgoing_message_to_connection(
        &self,
        request_context: Option<RequestContext>,
        connection_id: ConnectionId,
        message: OutgoingMessage,
        message_kind: &'static str,
    ) {
        let send_fut = self.sender.send(OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx: None,
        });
        let send_result = if let Some(request_context) = request_context {
            send_fut.instrument(request_context.span()).await
        } else {
            send_fut.await
        };

        if let Err(err) = send_result {
            warn!("failed to send {message_kind} to client: {err:?}");
        }
    }
}

async fn collect_turn_delivery_outcomes(
    receipts: Vec<PendingTurnDeliveryReceipt>,
    dispatch_started: Instant,
    receipt_deadline: tokio::time::Instant,
    post_core_dispatch_latency_ms: Option<u64>,
    delivery_shutdown: CancellationToken,
) -> Vec<TurnDeliveryOutcome> {
    let mut pending = FuturesUnordered::new();
    for receipt in receipts {
        let delivery_shutdown = delivery_shutdown.clone();
        pending.push(async move {
            if let Some(kind) = receipt.immediate_outcome {
                return TurnDeliveryOutcome {
                    connection_id: receipt.connection_id,
                    kind,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                };
            }
            let Some(receiver) = receipt.receiver else {
                return TurnDeliveryOutcome {
                    connection_id: receipt.connection_id,
                    kind: TurnDeliveryOutcomeKind::Failure,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                };
            };
            let receipt_result = tokio::select! {
                biased;
                result = tokio::time::timeout_at(receipt_deadline, receiver) => Some(result),
                _ = delivery_shutdown.cancelled() => None,
            };
            match receipt_result {
                Some(Ok(Ok(()))) => {
                    let successful_elapsed_ms = elapsed_millis(dispatch_started);
                    TurnDeliveryOutcome {
                        connection_id: receipt.connection_id,
                        kind: TurnDeliveryOutcomeKind::Success,
                        successful_elapsed_ms: Some(successful_elapsed_ms),
                        post_core_delivery_latency_ms: post_core_dispatch_latency_ms.map(
                            |dispatch_latency_ms| {
                                dispatch_latency_ms.saturating_add(successful_elapsed_ms)
                            },
                        ),
                    }
                }
                Some(Ok(Err(_))) => TurnDeliveryOutcome {
                    connection_id: receipt.connection_id,
                    kind: TurnDeliveryOutcomeKind::Failure,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                },
                Some(Err(_)) => TurnDeliveryOutcome {
                    connection_id: receipt.connection_id,
                    kind: TurnDeliveryOutcomeKind::Timeout,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                },
                None => TurnDeliveryOutcome {
                    connection_id: receipt.connection_id,
                    kind: TurnDeliveryOutcomeKind::ShutdownCancelled,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                },
            }
        });
    }

    let mut outcomes = Vec::with_capacity(pending.len());
    while let Some(outcome) = pending.next().await {
        outcomes.push(outcome);
    }
    outcomes.sort_unstable_by_key(|outcome| outcome.connection_id.0);
    outcomes
}

fn aggregate_turn_delivery(
    thread_id: ThreadId,
    turn_id: String,
    origin_connection_id: Option<ConnectionId>,
    outcomes: Vec<TurnDeliveryOutcome>,
) -> TurnDeliveryFact {
    let origin_outcome = origin_connection_id.and_then(|origin| {
        outcomes
            .iter()
            .find(|outcome| outcome.connection_id == origin)
    });
    let origin_target_present = origin_outcome.is_some();
    let origin_delivery_status = match origin_outcome.map(|outcome| outcome.kind) {
        None => TurnDeliveryStatus::NotTargeted,
        Some(TurnDeliveryOutcomeKind::Success) => TurnDeliveryStatus::Success,
        Some(TurnDeliveryOutcomeKind::Failure) => TurnDeliveryStatus::Failure,
        Some(TurnDeliveryOutcomeKind::Timeout) => TurnDeliveryStatus::Timeout,
        Some(TurnDeliveryOutcomeKind::ShutdownCancelled) => TurnDeliveryStatus::ShutdownCancelled,
    };
    let success_count = outcomes
        .iter()
        .filter(|outcome| outcome.kind == TurnDeliveryOutcomeKind::Success)
        .count();
    let failure_count = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.kind,
                TurnDeliveryOutcomeKind::Failure | TurnDeliveryOutcomeKind::ShutdownCancelled
            )
        })
        .count();
    let timeout_count = outcomes
        .iter()
        .filter(|outcome| outcome.kind == TurnDeliveryOutcomeKind::Timeout)
        .count();
    let shutdown_cancelled_count = outcomes
        .iter()
        .filter(|outcome| outcome.kind == TurnDeliveryOutcomeKind::ShutdownCancelled)
        .count();
    let first_successful_elapsed_ms = outcomes
        .iter()
        .filter_map(|outcome| outcome.successful_elapsed_ms)
        .min();
    let last_successful_elapsed_ms = outcomes
        .iter()
        .filter_map(|outcome| outcome.successful_elapsed_ms)
        .max();
    let first_post_core_delivery_latency_ms = outcomes
        .iter()
        .filter_map(|outcome| outcome.post_core_delivery_latency_ms)
        .min();
    let last_post_core_delivery_latency_ms = outcomes
        .iter()
        .filter_map(|outcome| outcome.post_core_delivery_latency_ms)
        .max();

    TurnDeliveryFact {
        thread_id: thread_id.to_string(),
        turn_id,
        target_count: count_u32(outcomes.len()),
        success_count: count_u32(success_count),
        failure_count: count_u32(failure_count),
        timeout_count: count_u32(timeout_count),
        shutdown_cancelled_count: count_u32(shutdown_cancelled_count),
        origin_target_present,
        origin_delivery_status,
        origin_successful_elapsed_ms: origin_outcome
            .and_then(|outcome| outcome.successful_elapsed_ms),
        first_successful_elapsed_ms,
        last_successful_elapsed_ms,
        first_post_core_delivery_latency_ms,
        last_post_core_delivery_latency_ms,
    }
}

fn count_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn now_unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use codex_app_server_protocol::AccountLoginCompletedNotification;
    use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::ApplyPatchApprovalParams;
    use codex_app_server_protocol::AuthMode;
    use codex_app_server_protocol::CommandExecutionApprovalDecision;
    use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
    use codex_app_server_protocol::ConfigWarningNotification;
    use codex_app_server_protocol::DynamicToolCallParams;
    use codex_app_server_protocol::FileChangeRequestApprovalParams;
    use codex_app_server_protocol::GuardianWarningNotification;
    use codex_app_server_protocol::ModelRerouteReason;
    use codex_app_server_protocol::ModelReroutedNotification;
    use codex_app_server_protocol::ModelVerification;
    use codex_app_server_protocol::ModelVerificationNotification;
    use codex_app_server_protocol::RateLimitSnapshot;
    use codex_app_server_protocol::RateLimitWindow;
    use codex_app_server_protocol::RawResponseItemCompletedNotification;
    use codex_app_server_protocol::ServerResponse;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnItemsView;
    use codex_app_server_protocol::TurnModerationMetadataNotification;
    use codex_app_server_protocol::TurnStatus;
    use codex_protocol::ThreadId;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;
    use crate::thread_state::ConnectionCapabilities;
    use crate::thread_state::ResolvedDeliveryFilter;
    use crate::thread_state::ThreadStateManager;

    #[test]
    fn accepted_recipient_snapshot_fanout_clones_exactly_n_minus_one_times() {
        struct CountedClone(Arc<AtomicUsize>);

        impl Clone for CountedClone {
            fn clone(&self) -> Self {
                self.0.fetch_add(1, Ordering::SeqCst);
                Self(Arc::clone(&self.0))
            }
        }

        for recipients in 0..=4 {
            let clones = Arc::new(AtomicUsize::new(0));
            let values = CloneForAllButLast::new(CountedClone(Arc::clone(&clones)), recipients)
                .collect::<Vec<_>>();
            assert_eq!(values.len(), recipients);
            assert_eq!(clones.load(Ordering::SeqCst), recipients.saturating_sub(1));
        }
    }

    #[tokio::test]
    async fn raw_events_are_filtered_before_snapshot_fanout() {
        let manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let raw_connection = ConnectionId(1);
        let filtered_connection = ConnectionId(2);
        for connection_id in [raw_connection, filtered_connection] {
            manager
                .connection_initialized(connection_id, ConnectionCapabilities::default())
                .await;
        }
        manager
            .try_ensure_connection_subscribed(thread_id, raw_connection, true)
            .await
            .expect("raw subscription");
        manager
            .try_ensure_connection_subscribed(thread_id, filtered_connection, false)
            .await
            .expect("filtered subscription");

        let (tx, mut rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let scoped = ThreadScopedOutgoingMessageSender::from_snapshot(
            Arc::clone(&outgoing),
            manager.recipient_snapshot(thread_id).await,
            thread_id,
        );
        scoped
            .send_server_notification(ServerNotification::RawResponseItemCompleted(
                RawResponseItemCompletedNotification {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item: ResponseItem::Message {
                        id: None,
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText {
                            text: "done".to_string(),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                },
            ))
            .await;

        let envelope = rx.recv().await.expect("raw recipient envelope");
        assert!(matches!(
            envelope,
            OutgoingEnvelope::ToSnapshotAcceptedConnection { connection_id, .. }
                if connection_id == raw_connection
        ));
        assert!(rx.try_recv().is_err());
        outgoing.shutdown_delivery_tasks().await;
    }

    #[tokio::test]
    async fn terminal_receipts_are_created_only_for_snapshot_accepted_recipients() {
        let manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let accepted_connection = ConnectionId(1);
        let opted_out_connection = ConnectionId(2);
        manager
            .connection_initialized(accepted_connection, ConnectionCapabilities::default())
            .await;
        manager
            .connection_initialized(
                opted_out_connection,
                ConnectionCapabilities {
                    request_attestation: false,
                    delivery_filter: ResolvedDeliveryFilter::new(
                        false,
                        HashSet::from(["turn/completed".to_string()]),
                    ),
                },
            )
            .await;
        for connection_id in [accepted_connection, opted_out_connection] {
            assert!(
                manager
                    .try_add_connection_to_thread(thread_id, connection_id)
                    .await
            );
        }

        let (tx, mut rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let scoped = ThreadScopedOutgoingMessageSender::from_snapshot(
            Arc::clone(&outgoing),
            manager.recipient_snapshot(thread_id).await,
            thread_id,
        );
        scoped
            .send_server_notification_with_receipts(
                turn_completed_notification(thread_id, "turn-1"),
                None,
            )
            .await;

        let envelope = rx.recv().await.expect("accepted terminal recipient");
        let OutgoingEnvelope::ToSnapshotAcceptedConnection {
            connection_id,
            write_complete_tx,
            ..
        } = envelope
        else {
            panic!("terminal delivery should use the frozen snapshot decision");
        };
        assert_eq!(connection_id, accepted_connection);
        write_complete_tx
            .expect("accepted terminal delivery has a receipt")
            .send(())
            .expect("receipt collector is active");
        assert!(rx.try_recv().is_err());
        outgoing.shutdown_delivery_tasks().await;
    }

    fn turn_completed_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: thread_id.to_string(),
            turn: Turn {
                id: turn_id.to_string(),
                items: Vec::new(),
                items_view: TurnItemsView::NotLoaded,
                error: None,
                status: TurnStatus::Completed,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
            completion: None,
            timing: None,
        })
    }

    #[test]
    fn verify_server_notification_serialization() {
        let notification =
            ServerNotification::AccountLoginCompleted(AccountLoginCompletedNotification {
                login_id: Some(Uuid::nil().to_string()),
                success: true,
                error: None,
            });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "account/login/completed",
                "params": {
                    "loginId": Uuid::nil().to_string(),
                    "success": true,
                    "error": null,
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the strum macros serialize the method field correctly"),
            "ensure the strum macros serialize the method field correctly"
        );
    }

    #[test]
    fn verify_account_login_completed_notification_serialization() {
        let notification =
            ServerNotification::AccountLoginCompleted(AccountLoginCompletedNotification {
                login_id: Some(Uuid::nil().to_string()),
                success: true,
                error: None,
            });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "account/login/completed",
                "params": {
                    "loginId": Uuid::nil().to_string(),
                    "success": true,
                    "error": null,
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_rate_limits_notification_serialization() {
        let notification =
            ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
                rate_limits: RateLimitSnapshot {
                    limit_id: Some("codex".to_string()),
                    limit_name: None,
                    primary: Some(RateLimitWindow {
                        used_percent: 25,
                        window_duration_mins: Some(15),
                        resets_at: Some(123),
                    }),
                    secondary: None,
                    credits: None,
                    individual_limit: None,
                    plan_type: Some(PlanType::Plus),
                    rate_limit_reached_type: None,
                },
            });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "account/rateLimits/updated",
                "params": {
                        "rateLimits": {
                        "limitId": "codex",
                        "limitName": null,
                        "primary": {
                            "usedPercent": 25,
                            "windowDurationMins": 15,
                            "resetsAt": 123
                        },
                        "secondary": null,
                        "credits": null,
                        "individualLimit": null,
                        "planType": "plus",
                        "rateLimitReachedType": null
                    }
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_updated_notification_serialization() {
        let notification = ServerNotification::AccountUpdated(AccountUpdatedNotification {
            auth_mode: Some(AuthMode::ApiKey),
            plan_type: None,
        });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "account/updated",
                "params": {
                    "authMode": "apikey",
                    "planType": null
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_config_warning_notification_serialization() {
        let notification = ServerNotification::ConfigWarning(ConfigWarningNotification {
            summary: "Config error: using defaults".to_string(),
            details: Some("error loading config: bad config".to_string()),
            path: None,
            range: None,
        });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!( {
                "method": "configWarning",
                "params": {
                    "summary": "Config error: using defaults",
                    "details": "error loading config: bad config",
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_guardian_warning_notification_serialization() {
        let notification = ServerNotification::GuardianWarning(GuardianWarningNotification {
            thread_id: "thread-1".to_string(),
            message: "Automatic approval review denied the requested action.".to_string(),
        });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "guardianWarning",
                "params": {
                    "threadId": "thread-1",
                    "message": "Automatic approval review denied the requested action.",
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_model_rerouted_notification_serialization() {
        let notification = ServerNotification::ModelRerouted(ModelReroutedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            from_model: "gpt-5.3-codex".to_string(),
            to_model: "gpt-5.2".to_string(),
            reason: ModelRerouteReason::HighRiskCyberActivity,
        });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "model/rerouted",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "fromModel": "gpt-5.3-codex",
                    "toModel": "gpt-5.2",
                    "reason": "highRiskCyberActivity",
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_model_verification_notification_serialization() {
        let notification = ServerNotification::ModelVerification(ModelVerificationNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            verifications: vec![ModelVerification::TrustedAccessForCyber],
        });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "model/verification",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "verifications": ["trustedAccessForCyber"],
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_turn_moderation_metadata_notification_serialization() {
        let notification =
            ServerNotification::TurnModerationMetadata(TurnModerationMetadataNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                metadata: json!({"presentation": "inline"}),
            });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "turn/moderationMetadata",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "metadata": {"presentation": "inline"},
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn server_request_response_from_result_decodes_typed_response() {
        let request = ServerRequest::CommandExecutionRequestApproval {
            request_id: RequestId::Integer(7),
            params: CommandExecutionRequestApprovalParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                started_at_ms: 0,
                approval_id: None,
                environment_id: None,
                reason: None,
                network_approval_context: None,
                command: Some("echo hi".to_string()),
                cwd: None,
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: None,
            },
        };

        let response = request
            .response_from_result(json!({
                "decision": "acceptForSession",
            }))
            .expect("decode typed server response");

        let ServerResponse::CommandExecutionRequestApproval {
            request_id,
            response,
        } = response
        else {
            panic!("expected command execution approval response");
        };
        assert_eq!(request_id, RequestId::Integer(7));
        assert_eq!(
            response.decision,
            CommandExecutionApprovalDecision::AcceptForSession
        );
    }
    #[tokio::test]
    async fn send_response_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(42),
            request_id: RequestId::Integer(7),
        };

        outgoing
            .send_response(
                request_id.clone(),
                ClientResponsePayload::ThreadArchive(
                    codex_app_server_protocol::ThreadArchiveResponse {},
                ),
            )
            .await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                assert_eq!(connection_id, ConnectionId(42));
                let OutgoingMessage::Response(response) = message else {
                    panic!("expected response message");
                };
                assert_eq!(response.id, request_id.request_id);
                assert_eq!(response.result, json!({}));
            }
            other => panic!("expected targeted response envelope, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_response_clears_registered_request_context() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(42),
            request_id: RequestId::Integer(7),
        };

        outgoing
            .register_request_context(RequestContext::new(
                request_id.clone(),
                tracing::info_span!("app_server.request", rpc.method = "thread/start"),
                /*parent_trace*/ None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 1);

        outgoing
            .send_response(
                request_id,
                ClientResponsePayload::ThreadArchive(
                    codex_app_server_protocol::ThreadArchiveResponse {},
                ),
            )
            .await;

        assert_eq!(outgoing.request_context_count().await, 0);
    }

    #[tokio::test]
    async fn send_error_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId::Integer(3),
        };
        let error = internal_error("boom");

        outgoing.send_error(request_id.clone(), error.clone()).await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                assert_eq!(connection_id, ConnectionId(9));
                let OutgoingMessage::Error(outgoing_error) = message else {
                    panic!("expected error message");
                };
                assert_eq!(outgoing_error.id, RequestId::Integer(3));
                assert_eq!(outgoing_error.error, error);
            }
            other => panic!("expected targeted error envelope, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_server_notification_to_connection_and_wait_tracks_write_completion() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let send_task = tokio::spawn(async move {
            outgoing
                .send_server_notification_to_connection_and_wait(
                    ConnectionId(42),
                    ServerNotification::ModelRerouted(ModelReroutedNotification {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        from_model: "gpt-5.3-codex".to_string(),
                        to_model: "gpt-5.2".to_string(),
                        reason: ModelRerouteReason::HighRiskCyberActivity,
                    }),
                )
                .await
        });

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx,
        } = envelope
        else {
            panic!("expected targeted server notification envelope");
        };
        assert_eq!(connection_id, ConnectionId(42));
        assert!(matches!(message, OutgoingMessage::AppServerNotification(_)));
        write_complete_tx
            .expect("write completion sender should be attached")
            .send(())
            .expect("receiver should still be waiting");

        timeout(Duration::from_secs(1), send_task)
            .await
            .expect("send task should finish after write completion is signaled")
            .expect("send task should not panic");
    }

    #[tokio::test]
    async fn terminal_dispatch_freezes_sorts_and_deduplicates_targets() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let scoped = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(3), ConnectionId(1), ConnectionId(3)],
            thread_id,
        );

        scoped
            .send_server_notification_with_receipts(
                turn_completed_notification(thread_id, "turn-1"),
                Some(ConnectionId(3)),
            )
            .await;

        for expected_connection_id in [ConnectionId(1), ConnectionId(3)] {
            let envelope = timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("terminal dispatch should arrive before timeout")
                .expect("terminal dispatch channel should stay open");
            let OutgoingEnvelope::ToSnapshotAcceptedConnection {
                connection_id,
                message,
                write_complete_tx,
            } = envelope
            else {
                panic!("terminal notification must use targeted dispatch");
            };
            assert_eq!(connection_id, expected_connection_id);
            assert!(matches!(
                message,
                OutgoingMessage::AppServerNotification(ServerNotification::TurnCompleted(_))
            ));
            write_complete_tx
                .expect("terminal dispatch must request a writer receipt")
                .send(())
                .expect("receipt collector should still be waiting");
        }
        assert!(rx.try_recv().is_err(), "no duplicate dispatch is allowed");
        outgoing.shutdown_delivery_tasks().await;
    }

    #[test]
    fn turn_delivery_aggregation_preserves_origin_and_broadcast_semantics() {
        let thread_id = ThreadId::new();
        let fact = aggregate_turn_delivery(
            thread_id,
            "turn-1".to_string(),
            Some(ConnectionId(3)),
            vec![
                TurnDeliveryOutcome {
                    connection_id: ConnectionId(1),
                    kind: TurnDeliveryOutcomeKind::Success,
                    successful_elapsed_ms: Some(7),
                    post_core_delivery_latency_ms: Some(17),
                },
                TurnDeliveryOutcome {
                    connection_id: ConnectionId(2),
                    kind: TurnDeliveryOutcomeKind::Timeout,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                },
                TurnDeliveryOutcome {
                    connection_id: ConnectionId(3),
                    kind: TurnDeliveryOutcomeKind::Success,
                    successful_elapsed_ms: Some(11),
                    post_core_delivery_latency_ms: Some(21),
                },
                TurnDeliveryOutcome {
                    connection_id: ConnectionId(4),
                    kind: TurnDeliveryOutcomeKind::ShutdownCancelled,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                },
            ],
        );

        assert_eq!(fact.thread_id, thread_id.to_string());
        assert_eq!(fact.turn_id, "turn-1");
        assert_eq!(fact.target_count, 4);
        assert_eq!(fact.success_count, 2);
        assert_eq!(fact.failure_count, 1);
        assert_eq!(fact.timeout_count, 1);
        assert_eq!(fact.shutdown_cancelled_count, 1);
        assert!(fact.origin_target_present);
        assert_eq!(fact.origin_delivery_status, TurnDeliveryStatus::Success);
        assert_eq!(fact.origin_successful_elapsed_ms, Some(11));
        assert_eq!(fact.first_successful_elapsed_ms, Some(7));
        assert_eq!(fact.last_successful_elapsed_ms, Some(11));
        assert_eq!(fact.first_post_core_delivery_latency_ms, Some(17));
        assert_eq!(fact.last_post_core_delivery_latency_ms, Some(21));
    }

    #[tokio::test]
    async fn receipt_collection_marks_pending_targets_shutdown_cancelled() {
        let (_write_complete_tx, write_complete_rx) = oneshot::channel();
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let outcomes = collect_turn_delivery_outcomes(
            vec![PendingTurnDeliveryReceipt {
                connection_id: ConnectionId(9),
                receiver: Some(write_complete_rx),
                immediate_outcome: None,
            }],
            Instant::now(),
            tokio::time::Instant::now() + Duration::from_secs(30),
            Some(5),
            shutdown,
        )
        .await;

        assert_eq!(
            outcomes,
            vec![TurnDeliveryOutcome {
                connection_id: ConnectionId(9),
                kind: TurnDeliveryOutcomeKind::ShutdownCancelled,
                successful_elapsed_ms: None,
                post_core_delivery_latency_ms: None,
            }]
        );
    }

    #[tokio::test]
    async fn connection_closed_clears_registered_request_contexts() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let closed_connection_request = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId::Integer(3),
        };
        let open_connection_request = ConnectionRequestId {
            connection_id: ConnectionId(10),
            request_id: RequestId::Integer(4),
        };

        outgoing
            .register_request_context(RequestContext::new(
                closed_connection_request,
                tracing::info_span!("app_server.request", rpc.method = "turn/interrupt"),
                /*parent_trace*/ None,
            ))
            .await;
        outgoing
            .register_request_context(RequestContext::new(
                open_connection_request,
                tracing::info_span!("app_server.request", rpc.method = "turn/start"),
                /*parent_trace*/ None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 2);

        outgoing.connection_closed(ConnectionId(9)).await;

        assert_eq!(outgoing.request_context_count().await, 1);
    }

    #[tokio::test]
    async fn notify_client_error_forwards_error_to_waiter() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());

        let (request_id, wait_for_result) = outgoing
            .send_request(ServerRequestPayload::ApplyPatchApproval(
                ApplyPatchApprovalParams {
                    conversation_id: ThreadId::new(),
                    call_id: "call-id".to_string(),
                    file_changes: HashMap::new(),
                    reason: None,
                    grant_root: None,
                },
            ))
            .await;

        let error = internal_error("refresh failed");

        outgoing
            .notify_client_error(request_id, error.clone())
            .await;

        let result = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("wait should not time out")
            .expect("waiter should receive a callback");
        assert_eq!(result, Err(error));
    }

    #[tokio::test]
    async fn pending_requests_for_thread_returns_thread_requests_in_request_id_order() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );

        let (dynamic_tool_request_id, _dynamic_tool_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-0".to_string(),
                    namespace: None,
                    tool: "tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let (first_request_id, _first_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::ToolRequestUserInput(
                ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-1".to_string(),
                    questions: vec![],
                    auto_resolution_ms: None,
                },
            ))
            .await;
        let (second_request_id, _second_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::FileChangeRequestApproval(
                FileChangeRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-2".to_string(),
                    started_at_ms: 0,
                    reason: None,
                    grant_root: None,
                },
            ))
            .await;
        let pending_requests = outgoing.pending_requests_for_thread(thread_id).await;
        assert_eq!(
            pending_requests
                .iter()
                .map(ServerRequest::id)
                .collect::<Vec<_>>(),
            vec![
                &dynamic_tool_request_id,
                &first_request_id,
                &second_request_id
            ]
        );
    }

    #[tokio::test]
    async fn cancel_requests_for_thread_cancels_all_thread_requests() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );

        let (_dynamic_tool_request_id, dynamic_tool_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-0".to_string(),
                    namespace: None,
                    tool: "tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let (_request_id, user_input_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::ToolRequestUserInput(
                ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-1".to_string(),
                    questions: vec![],
                    auto_resolution_ms: None,
                },
            ))
            .await;
        let error = internal_error("tracked request cancelled");

        outgoing
            .cancel_requests_for_thread(thread_id, Some(error.clone()))
            .await;

        let dynamic_tool_result = timeout(Duration::from_secs(1), dynamic_tool_waiter)
            .await
            .expect("dynamic tool waiter should resolve")
            .expect("dynamic tool waiter should receive a callback");
        let user_input_result = timeout(Duration::from_secs(1), user_input_waiter)
            .await
            .expect("user input waiter should resolve")
            .expect("user input waiter should receive a callback");
        assert_eq!(dynamic_tool_result, Err(error.clone()));
        assert_eq!(user_input_result, Err(error));
        assert!(
            outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty()
        );
    }
}
