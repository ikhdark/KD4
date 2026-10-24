use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
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
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestPayload;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
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
pub(crate) use codex_app_server_transport::ConnectionId;
pub(crate) use codex_app_server_transport::OutgoingError;
pub(crate) use codex_app_server_transport::OutgoingMessage;
pub(crate) use codex_app_server_transport::OutgoingResponse;
pub(crate) use codex_app_server_transport::QueuedOutgoingMessage;

#[cfg(test)]
use codex_protocol::account::PlanType;

pub(crate) type ClientRequestResult = std::result::Result<Result, JSONRPCErrorError>;

const TURN_DELIVERY_RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const TURN_DELIVERY_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const WRITER_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const RESOURCE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
const NO_ACTIVE_AUTHORIZED_CONNECTIONS_ERROR: &str =
    "client request canceled because no active authorized connections are available";
const NO_INITIALIZED_CONNECTIONS_ERROR: &str =
    "client request canceled because no initialized connections are available";
const ALL_AUTHORIZED_CONNECTIONS_DISCONNECTED_ERROR: &str =
    "client request canceled because all authorized connections disconnected";
const ALL_AUTHORIZED_CONNECTION_SENDS_FAILED_ERROR: &str =
    "client request canceled because delivery failed for all authorized connections";
const LEGACY_EXEC_COMMAND_APPROVAL_EMISSION_ERROR: &str =
    "legacy execCommandApproval is decode-only; use item/commandExecution/requestApproval";

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
    generation: Arc<()>,
    span: Span,
    parent_trace: Option<W3cTraceContext>,
    work_lease: Option<Arc<RequestWorkLease>>,
}

impl RequestContext {
    pub(crate) fn new(
        request_id: ConnectionRequestId,
        span: Span,
        parent_trace: Option<W3cTraceContext>,
    ) -> Self {
        Self {
            request_id,
            generation: Arc::new(()),
            span,
            parent_trace,
            work_lease: None,
        }
    }

    pub(crate) fn request_trace(&self) -> Option<W3cTraceContext> {
        span_w3c_trace_context(&self.span).or_else(|| self.parent_trace.clone())
    }

    pub(crate) fn span(&self) -> Span {
        self.span.clone()
    }

    pub(crate) fn request_id(&self) -> &ConnectionRequestId {
        &self.request_id
    }

    fn record_turn_id(&self, turn_id: &str) {
        self.span.record("turn.id", turn_id);
    }
}

#[derive(Debug)]
pub(crate) struct OutgoingWriteReceipt {
    pub(crate) writer: oneshot::Sender<Instant>,
    pub(crate) filtered: Option<oneshot::Sender<()>>,
}

impl From<oneshot::Sender<Instant>> for OutgoingWriteReceipt {
    fn from(writer: oneshot::Sender<Instant>) -> Self {
        Self {
            writer,
            filtered: None,
        }
    }
}

impl OutgoingWriteReceipt {
    #[cfg(test)]
    pub(crate) fn send(self, at: Instant) -> std::result::Result<(), Instant> {
        self.writer.send(at)
    }
}

#[derive(Debug)]
pub(crate) enum OutgoingEnvelope {
    ToConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
        write_complete_tx: Option<OutgoingWriteReceipt>,
    },
    Broadcast {
        message: OutgoingMessage,
    },
}

// Shared by the handler and final-response context, including deferred replies.
struct RequestWorkLease {
    _global: tokio::sync::OwnedSemaphorePermit,
    _connection: tokio::sync::OwnedSemaphorePermit,
    _global_bytes: tokio::sync::OwnedSemaphorePermit,
    _connection_bytes: tokio::sync::OwnedSemaphorePermit,
}

/// Sends messages to the client and manages request callbacks.
pub(crate) struct OutgoingMessageSender {
    outstanding: Arc<tokio::sync::Semaphore>,
    outstanding_control: Arc<tokio::sync::Semaphore>,
    outstanding_bytes: Arc<tokio::sync::Semaphore>,
    next_server_request_id: AtomicI64,
    rejection_slots: Arc<tokio::sync::Semaphore>,
    sender: mpsc::Sender<OutgoingEnvelope>,
    notification_sender: Option<mpsc::Sender<OutgoingEnvelope>>,
    active_connections: Arc<Mutex<HashMap<ConnectionId, ActiveConnection>>>,
    dynamic_tool_owners: Mutex<HashMap<ThreadId, ConnectionId>>,
    request_id_to_callback: Arc<StdMutex<HashMap<RequestId, PendingCallbackEntry>>>,
    /// Incoming requests that are still waiting on a final response or error.
    /// We keep them here because this is where responses, errors, and
    /// disconnect cleanup all get handled.
    request_contexts: Mutex<HashMap<ConnectionRequestId, RequestContext>>,
    component_notification_cache: Mutex<ComponentNotificationCache>,
    analytics_events_client: AnalyticsEventsClient,
    delivery_tasks: TaskTracker,
    delivery_shutdown: CancellationToken,
    delivery_accepting: Mutex<bool>,
}

struct ActiveConnection {
    outstanding: Arc<tokio::sync::Semaphore>,
    outstanding_control: Arc<tokio::sync::Semaphore>,
    outstanding_bytes: Arc<tokio::sync::Semaphore>,
    initialized: Arc<AtomicBool>,
    delivery_failure: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct ThreadScopedOutgoingMessageSender {
    outgoing: Arc<OutgoingMessageSender>,
    connection_ids: Arc<Vec<ConnectionId>>,
    experimental_api_connection_ids: Arc<Vec<ConnectionId>>,
    thread_id: ThreadId,
}

#[derive(Default)]
struct ComponentNotificationCache {
    token_usage: HashMap<(ConnectionId, String), ThreadTokenUsageUpdatedNotification>,
    rate_limits: HashMap<ConnectionId, RateLimitSnapshot>,
    publications: HashMap<ComponentNotificationKey, ComponentNotificationPublication>,
}

struct ComponentNotificationPublication {
    generation: Arc<()>,
    forced: bool,
    notification: Arc<ServerNotification>,
}

impl ComponentNotificationPublication {
    fn matches(&self, notification: &ServerNotification) -> bool {
        match (&*self.notification, notification) {
            (ServerNotification::ThreadTokenUsageUpdated(left), ServerNotification::ThreadTokenUsageUpdated(right)) => left == right,
            (ServerNotification::AccountRateLimitsUpdated(left), ServerNotification::AccountRateLimitsUpdated(right)) => left.rate_limits == right.rate_limits,
            _ => false,
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum ComponentNotificationKey {
    TokenUsage(ConnectionId, String),
    RateLimits(ConnectionId),
}

impl ComponentNotificationKey {
    fn new(connection_id: ConnectionId, notification: &ServerNotification) -> Option<Self> {
        match notification {
            ServerNotification::ThreadTokenUsageUpdated(notification) => {
                Some(Self::TokenUsage(connection_id, notification.thread_id.clone()))
            }
            ServerNotification::AccountRateLimitsUpdated(_) => Some(Self::RateLimits(connection_id)),
            _ => None,
        }
    }

    fn connection_id(&self) -> ConnectionId {
        match self {
            Self::TokenUsage(connection_id, _) | Self::RateLimits(connection_id) => *connection_id,
        }
    }
}

impl ComponentNotificationCache {
    fn begin_publication(
        &mut self,
        connection_ids: &[ConnectionId],
        notification: &ServerNotification,
        force: bool,
    ) -> Arc<()> {
        let generation = Arc::new(());
        let mut snapshot = None;
        for connection_id in connection_ids {
            if let Some(key) = ComponentNotificationKey::new(*connection_id, notification) {
                if !force && !self.changed(*connection_id, notification) {
                    match self.publications.get(&key) {
                        // An identical live update must not cancel an explicit
                        // replay that is still waiting for transport capacity.
                        Some(pending) if pending.forced && pending.matches(notification) => continue,
                        None => continue,
                        _ => {}
                    }
                }
                let notification = snapshot.get_or_insert_with(|| Arc::new(notification.clone()));
                self.publications.insert(key, ComponentNotificationPublication {
                    generation: Arc::clone(&generation),
                    forced: force,
                    notification: Arc::clone(notification),
                });
            }
        }
        generation
    }

    fn should_publish(
        &self,
        connection_id: ConnectionId,
        notification: &ServerNotification,
        generation: &Arc<()>,
        force: bool,
    ) -> bool {
        if let Some(key) = ComponentNotificationKey::new(connection_id, notification)
            && !self.publications.get(&key).is_some_and(|current| Arc::ptr_eq(&current.generation, generation))
        {
            return false;
        }
        force || self.changed(connection_id, notification)
    }

    fn changed(&self, connection_id: ConnectionId, notification: &ServerNotification) -> bool {
        match notification {
            ServerNotification::ThreadTokenUsageUpdated(notification) => {
                self.token_usage.get(&(connection_id, notification.thread_id.clone())) != Some(notification)
            }
            ServerNotification::AccountRateLimitsUpdated(notification) => {
                self.rate_limits.get(&connection_id) != Some(&notification.rate_limits)
            }
            _ => true,
        }
    }

    fn record_publication(&mut self, connection_id: ConnectionId, notification: &ServerNotification) {
        match notification {
            ServerNotification::ThreadTokenUsageUpdated(notification) => {
                self.token_usage.insert((connection_id, notification.thread_id.clone()), notification.clone());
            }
            ServerNotification::AccountRateLimitsUpdated(notification) => {
                self.rate_limits.insert(connection_id, notification.rate_limits.clone());
            }
            _ => {}
        }
        if let Some(key) = ComponentNotificationKey::new(connection_id, notification) {
            self.publications.remove(&key);
        }
    }

    fn remove_connection(&mut self, connection_id: ConnectionId) {
        self.token_usage
            .retain(|(cached_connection_id, _), _| *cached_connection_id != connection_id);
        self.rate_limits.remove(&connection_id);
        self.publications.retain(|key, _| key.connection_id() != connection_id);
    }

    fn remove_thread(&mut self, thread_id: &str) {
        self.token_usage
            .retain(|(_, cached_thread_id), _| cached_thread_id != thread_id);
        self.publications.retain(|key, _| {
            !matches!(key, ComponentNotificationKey::TokenUsage(_, cached_thread_id) if cached_thread_id == thread_id)
        });
    }
}

struct PendingCallbackEntry {
    // Every terminal removal drops this guard, including cancellation cleanup.
    _settlement: tokio_util::sync::DropGuard,
    settlement: CancellationToken,
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
    request: ServerRequest,
    /// Connections that received this request and may resolve its callback.
    connection_ids: HashSet<ConnectionId>,
    /// Successful initial or replay publication, shared across fanout owners.
    delivered_connection_ids: HashSet<ConnectionId>,
}

struct PendingCallbackRegistration {
    callbacks: Arc<StdMutex<HashMap<RequestId, PendingCallbackEntry>>>,
    request_id: Option<RequestId>,
}

impl PendingCallbackRegistration {
    fn disarm(&mut self) {
        self.request_id = None;
    }
}

impl Drop for PendingCallbackRegistration {
    fn drop(&mut self) {
        let Some(request_id) = self.request_id.take() else {
            return;
        };
        let removed = {
            self.callbacks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&request_id)
        };
        // Drop the sender after releasing the registry mutex; cancellation must
        // not depend on another async task or the originating runtime surviving.
        drop(removed);
    }
}

enum TakeCallbackResult {
    Taken(RequestId, Box<PendingCallbackEntry>),
    Unauthorized,
    Missing,
}

struct PendingTurnDeliveryReceipt {
    connection_id: ConnectionId,
    receiver: Option<oneshot::Receiver<Instant>>,
    filtered: Option<oneshot::Receiver<()>>,
    immediate_outcome: Option<TurnDeliveryOutcomeKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnDeliveryOutcomeKind {
    Success,
    Filtered,
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

        let ServerNotification::TurnCompleted(completed) = &notification else {
            if !self.connection_ids.is_empty() {
                self.outgoing
                    .send_server_notification_to_connections(
                        self.connection_ids.as_slice(),
                        notification,
                    )
                    .await;
            }
            return;
        };
        let turn_id = completed.turn.id.clone();
        let core_completed_at = completed
            .timing
            .as_ref()
            .and_then(|timing| timing.activity_timeline.as_ref())
            .and_then(codex_protocol::timing::TurnTimingTimeline::local_end_instant);
        // Legacy/coarse wall epochs cannot prove same-domain elapsed. Keep
        // that latency unknown; writer-relative elapsed remains observable.

        let mut target_connection_ids = self.connection_ids.as_ref().clone();
        target_connection_ids.sort_unstable_by_key(|connection_id| connection_id.0);
        target_connection_ids.dedup();
        self.outgoing
            .dispatch_turn_completed_with_receipts(
                self.thread_id,
                turn_id,
                target_connection_ids,
                origin_connection_id,
                core_completed_at,
                notification,
            )
            .await;
    }

    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        connection_ids: Vec<ConnectionId>,
        thread_id: ThreadId,
    ) -> Self {
        Self::new_with_experimental_api_connections(
            outgoing,
            connection_ids.clone(),
            connection_ids,
            thread_id,
        )
    }

    pub(crate) fn new_with_experimental_api_connections(
        outgoing: Arc<OutgoingMessageSender>,
        connection_ids: Vec<ConnectionId>,
        experimental_api_connection_ids: Vec<ConnectionId>,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            outgoing,
            connection_ids: Arc::new(connection_ids),
            experimental_api_connection_ids: Arc::new(experimental_api_connection_ids),
            thread_id,
        }
    }

    /// Failed admission has no callback ID, but still completes the response path.
    pub(crate) async fn send_request(
        &self,
        payload: ServerRequestPayload,
    ) -> (Option<RequestId>, oneshot::Receiver<ClientRequestResult>) {
        let connection_ids = match &payload {
            ServerRequestPayload::DynamicToolCall(_) => {
                self.experimental_api_connection_ids.as_slice()
            }
            _ => self.connection_ids.as_slice(),
        };
        match self
            .outgoing
            .send_request_to_connections(Some(connection_ids), payload, Some(self.thread_id))
            .await
        {
            Ok((id, receiver)) => (Some(id), receiver),
            Err(error) => {
                let (sender, receiver) = oneshot::channel();
                let _ = sender.send(Err(error));
                (None, receiver)
            }
        }
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
        if self.connection_ids.is_empty() {
            return;
        }
        self.outgoing
            .send_server_notification_to_connections(self.connection_ids.as_slice(), notification)
            .await;
    }

    pub(crate) async fn send_component_notification_if_changed(
        &self,
        notification: ServerNotification,
    ) {
        if self
            .outgoing
            .send_component_notification_to_connections(
                self.connection_ids.as_slice(),
                &notification,
                false,
            )
            .await
        {
            self.outgoing
                .analytics_events_client
                .track_notification(&notification);
        }
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

    pub(crate) async fn send_response_before<T>(
        &self,
        request_id: ConnectionRequestId,
        response: T,
        deadline: tokio::time::Instant,
    ) where
        T: Into<ClientResponsePayload>,
    {
        let mut context = None;
        if tokio::time::timeout_at(deadline, async {
            context = self.outgoing.request_context(&request_id).await;
            self.outgoing.send_response(request_id.clone(), response).await;
        }).await.is_err()
        {
            self.outgoing.fail_connection_delivery(request_id.connection_id).await;
            self.outgoing.finish_request_context(context.as_ref()).await;
        }
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
            outstanding: Arc::new(tokio::sync::Semaphore::new(1024)),
            outstanding_control: Arc::new(tokio::sync::Semaphore::new(64)),
            outstanding_bytes: Arc::new(tokio::sync::Semaphore::new(32 * 1024 * 1024)),
            next_server_request_id: AtomicI64::new(0),
            rejection_slots: Arc::new(tokio::sync::Semaphore::new(64)),
            sender,
            notification_sender: None,
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            dynamic_tool_owners: Mutex::new(HashMap::new()),
            request_id_to_callback: Arc::new(StdMutex::new(HashMap::new())),
            request_contexts: Mutex::new(HashMap::new()),
            component_notification_cache: Mutex::new(ComponentNotificationCache::default()),
            analytics_events_client,
            delivery_tasks: TaskTracker::new(),
            delivery_shutdown: CancellationToken::new(),
            delivery_accepting: Mutex::new(true),
        }
    }

    pub(crate) fn with_notification_sender(
        mut self,
        sender: mpsc::Sender<OutgoingEnvelope>,
    ) -> Self {
        self.notification_sender = Some(sender);
        self
    }

    fn notification_sender(&self) -> &mpsc::Sender<OutgoingEnvelope> {
        self.notification_sender.as_ref().unwrap_or(&self.sender)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Register admitted work before connection teardown can purge the connection's contexts"
    )]
    pub(crate) async fn admit_request_work(
        &self,
        context: &mut RequestContext,
        bytes: usize,
        control: bool,
    ) -> bool {
        // Controls have an independent finite envelope: at most 64 global / 8
        // connection owners, each no larger than 64 KiB (4 MiB / 512 KiB).
        // They must never depend on the saturated ordinary byte semaphores.
        if control && bytes > 64 * 1024 {
            return false;
        }
        let bytes = if control { 0 } else { bytes };
        let Ok(bytes) = u32::try_from(bytes) else {
            return false;
        };
        let connections = self.active_connections.lock().await;
        let Some(connection) = connections.get(&context.request_id.connection_id) else {
            return false;
        };
        let (global, local) = if control {
            (&self.outstanding_control, &connection.outstanding_control)
        } else {
            (&self.outstanding, &connection.outstanding)
        };
        let Ok(global) = Arc::clone(global).try_acquire_owned() else {
            return false;
        };
        let Ok(local) = Arc::clone(local).try_acquire_owned() else {
            return false;
        };
        let Ok(global_bytes) = Arc::clone(&self.outstanding_bytes).try_acquire_many_owned(bytes)
        else {
            return false;
        };
        let Ok(local_bytes) =
            Arc::clone(&connection.outstanding_bytes).try_acquire_many_owned(bytes)
        else {
            return false;
        };
        let mut request_contexts = self.request_contexts.lock().await;
        if request_contexts.get(&context.request_id)
            .is_some_and(|current| !Arc::ptr_eq(&current.generation, &context.generation))
        {
            return false;
        }
        context.work_lease = Some(Arc::new(RequestWorkLease {
            _global: global,
            _connection: local,
            _global_bytes: global_bytes,
            _connection_bytes: local_bytes,
        }));
        request_contexts.insert(context.request_id.clone(), context.clone());
        true
    }

    pub(crate) async fn register_dynamic_tool_owner(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) {
        self.dynamic_tool_owners
            .lock()
            .await
            .insert(thread_id, connection_id);
    }

    pub(crate) async fn forget_dynamic_tool_owner(&self, thread_id: ThreadId) {
        self.dynamic_tool_owners.lock().await.remove(&thread_id);
    }

    pub(crate) async fn register_request_context(&self, request_context: RequestContext) -> bool {
        if self.active_connections.lock().await
            .get(&request_context.request_id.connection_id)
            .is_some_and(|connection| connection.delivery_failure.is_cancelled())
        {
            return false;
        }
        let mut request_contexts = self.request_contexts.lock().await;
        match request_contexts.entry(request_context.request_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(request_context);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    #[cfg(test)]
    pub(crate) async fn connection_opened(
        &self,
        connection_id: ConnectionId,
        initialized: Arc<AtomicBool>,
    ) {
        self.connection_opened_with_runtime(connection_id, initialized, CancellationToken::new())
            .await;
    }

    pub(crate) async fn connection_opened_with_runtime(
        &self,
        connection_id: ConnectionId,
        initialized: Arc<AtomicBool>,
        delivery_failure: CancellationToken,
    ) {
        self.component_notification_cache
            .lock()
            .await
            .remove_connection(connection_id);
        self.active_connections.lock().await.insert(
            connection_id,
            ActiveConnection {
                outstanding: Arc::new(tokio::sync::Semaphore::new(64)),
                outstanding_control: Arc::new(tokio::sync::Semaphore::new(8)),
                // A maximum-size fs/writeFile request includes base64 expansion.
                outstanding_bytes: Arc::new(tokio::sync::Semaphore::new(16 * 1024 * 1024)),
                initialized,
                delivery_failure,
            },
        );
    }

    pub(crate) async fn fail_all_connection_delivery(&self) {
        for connection in self.active_connections.lock().await.values() {
            connection.initialized.store(false, Ordering::Release);
            connection.delivery_failure.cancel();
        }
    }

    pub(crate) async fn fail_connection_delivery(&self, connection_id: ConnectionId) {
        if let Some(connection) = self.active_connections.lock().await.get(&connection_id) {
            connection.initialized.store(false, Ordering::Release);
            connection.delivery_failure.cancel();
        }
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let orphaned_entries = {
            // Registration and replay use the same active -> callbacks lock order.
            // Removing the connection while holding the active lock prevents a
            // later registration from authorizing only this closed connection.
            let mut active_connections = Arc::clone(&self.active_connections).lock_owned().await;
            if let Some(connection) = active_connections.remove(&connection_id) {
                connection.delivery_failure.cancel();
            }
            let mut request_id_to_callback = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let orphaned_request_ids = request_id_to_callback
                .iter_mut()
                .filter_map(|(request_id, entry)| {
                    entry.delivered_connection_ids.remove(&connection_id);
                    let was_authorized = entry.connection_ids.remove(&connection_id);
                    (was_authorized && entry.connection_ids.is_empty())
                        .then_some(request_id.clone())
                })
                .collect::<Vec<_>>();
            orphaned_request_ids
                .into_iter()
                .filter_map(|request_id| request_id_to_callback.remove(&request_id))
                .collect::<Vec<_>>()
        };
        {
            let mut request_contexts = self.request_contexts.lock().await;
            request_contexts.retain(|request_id, _| request_id.connection_id != connection_id);
        }
        self.component_notification_cache
            .lock()
            .await
            .remove_connection(connection_id);
        for entry in orphaned_entries {
            let request_id = entry.request.id().clone();
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), request_id.clone());
            if let Err(err) = entry.callback.send(Err(internal_error(
                ALL_AUTHORIZED_CONNECTIONS_DISCONNECTED_ERROR,
            ))) {
                warn!("could not notify callback for {request_id:?} due to: {err:?}");
            }
        }
        self.analytics_events_client
            .track_connection_closed(connection_id.0);
    }

    async fn send_component_notification_to_connections(
        &self,
        connection_ids: &[ConnectionId],
        notification: &ServerNotification,
        force: bool,
    ) -> bool {
        if connection_ids.is_empty() {
            return false;
        }
        // Register every recipient before awaiting capacity. A later update to
        // c2 supersedes this publication even if c1 stalls the fanout first.
        let generation = self.component_notification_cache
            .lock()
            .await
            .begin_publication(connection_ids, notification, force);
        let deadline = tokio::time::Instant::now() + RESOURCE_DELIVERY_TIMEOUT;
        let mut published = false;
        for connection_id in connection_ids {
            if !self.component_notification_cache.lock().await
                .should_publish(*connection_id, notification, &generation, force)
            {
                continue;
            }
            let route_failed = self.active_connections.lock().await
                .get(connection_id).map(|connection| connection.delivery_failure.clone())
                .unwrap_or_default();
            let permit = tokio::select! {
                biased;
                _ = route_failed.cancelled() => None,
                _ = self.delivery_shutdown.cancelled() => None,
                result = tokio::time::timeout_at(deadline, self.notification_sender().reserve()) => {
                    match result {
                        Ok(Ok(permit)) => Some(permit),
                        _ => None,
                    }
                }
            };
            let Some(permit) = permit else {
                warn!(?connection_id, "component notification delivery failed or exceeded its budget");
                self.fail_connection_delivery(*connection_id).await;
                continue;
            };
            let mut cache = self.component_notification_cache.lock().await;
            if !route_failed.is_cancelled() && !self.delivery_shutdown.is_cancelled()
                && cache.should_publish(*connection_id, notification, &generation, force)
            {
                // The committed cache and synchronous enqueue share one short
                // critical section. Cancellation before admission changes neither.
                permit.send(OutgoingEnvelope::ToConnection {
                    connection_id: *connection_id,
                    message: OutgoingMessage::AppServerNotification(notification.clone()),
                    write_complete_tx: None,
                });
                cache.record_publication(*connection_id, notification);
                published = true;
            }
        }
        published
    }

    pub(crate) async fn send_initial_component_notification_to_connection(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
    ) {
        self.send_component_notification_to_connections(&[connection_id], &notification, true)
            .await;
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

    async fn request_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<RequestContext> {
        self.request_contexts.lock().await.get(request_id).cloned()
    }

    async fn finish_request_context(&self, context: Option<&RequestContext>) {
        let Some(context) = context else {
            return;
        };
        let mut request_contexts = self.request_contexts.lock().await;
        if let std::collections::hash_map::Entry::Occupied(entry) =
            request_contexts.entry(context.request_id.clone())
            && Arc::ptr_eq(&entry.get().generation, &context.generation)
        {
            entry.remove();
        }
    }

    #[cfg(test)]
    pub(crate) async fn lock_request_contexts_for_test(
        &self,
    ) -> tokio::sync::MutexGuard<'_, HashMap<ConnectionRequestId, RequestContext>> {
        self.request_contexts.lock().await
    }

    #[cfg(test)]
    async fn request_context_count(&self) -> usize {
        self.request_contexts.lock().await.len()
    }

    #[cfg(test)]
    pub(crate) async fn pending_callback_count(&self) -> usize {
        self.request_id_to_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    pub(crate) async fn send_request(
        &self,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.send_request_to_connections(
            /*connection_ids*/ None, request, /*thread_id*/ None,
        )
        .await
        .expect("test request ID should be available")
    }

    fn next_request_id(&self) -> std::result::Result<RequestId, JSONRPCErrorError> {
        self.next_server_request_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map(RequestId::Integer)
            .map_err(|_| internal_error("app-server request IDs are exhausted"))
    }

    /// Keep callback cleanup owned by the operation during both delivery and
    /// response waiting, including when a caller's deadline drops this future.
    pub(crate) async fn send_request_to_connections_and_wait(
        &self,
        connection_ids: Option<&[ConnectionId]>,
        request: ServerRequestPayload,
        thread_id: Option<ThreadId>,
    ) -> std::result::Result<ClientRequestResult, oneshot::error::RecvError> {
        let (request_id, receiver) = match self
            .send_request_to_connections(connection_ids, request, thread_id)
            .await
        {
            Ok(pending) => pending,
            Err(error) => return Ok(Err(error)),
        };
        let _registration = PendingCallbackRegistration {
            callbacks: Arc::clone(&self.request_id_to_callback),
            request_id: Some(request_id),
        };
        receiver.await
    }

    pub(crate) async fn send_request_to_connections(
        &self,
        connection_ids: Option<&[ConnectionId]>,
        request: ServerRequestPayload,
        thread_id: Option<ThreadId>,
    ) -> std::result::Result<(RequestId, oneshot::Receiver<ClientRequestResult>), JSONRPCErrorError>
    {
        let id = self.next_request_id()?;
        let outgoing_message_id = id.clone();
        if matches!(&request, ServerRequestPayload::ExecCommandApproval(_)) {
            let (tx_approve, rx_approve) = oneshot::channel();
            let _ = tx_approve.send(Err(internal_error(
                LEGACY_EXEC_COMMAND_APPROVAL_EMISSION_ERROR,
            )));
            return Ok((outgoing_message_id, rx_approve));
        }
        let request = request.request_with_id(outgoing_message_id.clone());
        let dynamic_owner = match thread_id {
            Some(thread_id) => self
                .dynamic_tool_owners
                .lock()
                .await
                .get(&thread_id)
                .copied(),
            None => None,
        };
        let explicit_recipients = connection_ids.is_some();
        // Keep the active-connection snapshot locked through callback registration.
        // Explicit target sets are only a capability/subscription filter; transport
        // liveness remains authoritative here.
        let active_connections = Arc::clone(&self.active_connections).lock_owned().await;
        let mut target_connection_ids = match connection_ids {
            Some(connection_ids) => connection_ids
                .iter()
                .copied()
                .filter(|connection_id| {
                    active_connections
                        .get(connection_id)
                        .is_some_and(|connection| connection.initialized.load(Ordering::Acquire))
                })
                .collect::<Vec<_>>(),
            None => active_connections
                .iter()
                .filter_map(|(connection_id, initialized)| {
                    initialized
                        .initialized
                        .load(Ordering::Acquire)
                        .then_some(*connection_id)
                })
                .collect(),
        };
        if matches!(request, ServerRequest::DynamicToolCall { .. }) {
            target_connection_ids.sort_unstable_by_key(|id| id.0);
            target_connection_ids.dedup();
            if let Some(owner) = dynamic_owner {
                target_connection_ids.retain(|id| *id == owner);
            } else if target_connection_ids.len() != 1 {
                // Legacy/resumed registrations can use a sole eligible client,
                // but subscription alone cannot choose among several executors.
                target_connection_ids.clear();
            }
        }
        let authorized_connection_ids = target_connection_ids.iter().copied().collect();

        let (tx_approve, rx_approve) = oneshot::channel();
        if target_connection_ids.is_empty() {
            let message = if explicit_recipients {
                NO_ACTIVE_AUTHORIZED_CONNECTIONS_ERROR
            } else {
                NO_INITIALIZED_CONNECTIONS_ERROR
            };
            let _ = tx_approve.send(Err(internal_error(message)));
            return Ok((outgoing_message_id, rx_approve));
        }
        let settlement = CancellationToken::new();
        {
            let mut request_id_to_callback = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            request_id_to_callback.insert(
                id,
                PendingCallbackEntry {
                    _settlement: settlement.clone().drop_guard(),
                    settlement: settlement.clone(),
                    callback: tx_approve,
                    thread_id,
                    request: request.clone(),
                    connection_ids: authorized_connection_ids,
                    delivered_connection_ids: HashSet::new(),
                },
            );
        }
        // From registration until this method returns, every await is a
        // cancellation point. Remove the callback if the send future is
        // dropped before its receiver can take ownership of that cleanup.
        let mut pending_registration = PendingCallbackRegistration {
            callbacks: Arc::clone(&self.request_id_to_callback),
            request_id: Some(outgoing_message_id.clone()),
        };
        drop(active_connections);

        let mut send_error = None;
        let mut analytics_connection_id = None;
        let delivery_deadline = tokio::time::Instant::now() + RESOURCE_DELIVERY_TIMEOUT;
        for connection_id in target_connection_ids {
            let permit = tokio::select! {
                biased;
                _ = settlement.cancelled() => break,
                _ = self.delivery_shutdown.cancelled() => {
                    send_error = Some("request delivery was cancelled by shutdown");
                    break;
                }
                result = tokio::time::timeout_at(delivery_deadline, self.sender.reserve()) => {
                    match result {
                        Ok(Ok(permit)) => permit,
                        Ok(Err(_)) => {
                            send_error = Some("request output channel is closed");
                            break;
                        }
                        Err(_) => {
                            send_error = Some("request delivery exceeded the resource delivery budget");
                            break;
                        }
                    }
                }
            };
            // Cancellation and recipient disconnect can remove this authority
            // while capacity is unavailable. Publish only while it remains live,
            // using the same synchronous callback fence as replay.
            let mut callbacks = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(entry) = callbacks.get_mut(&outgoing_message_id) else {
                break;
            };
            if !entry.connection_ids.contains(&connection_id) {
                continue;
            }
            entry.delivered_connection_ids.insert(connection_id);
            permit.send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::Request(request.clone()),
                write_complete_tx: None,
            });
            analytics_connection_id.get_or_insert(connection_id);
        }
        if let Some(connection_id) = analytics_connection_id {
            self.analytics_events_client
                .track_server_request(connection_id.0, &request);
        }
        let send_result = match send_error {
            Some(err) => Err(err),
            None => Ok(()),
        };

        if let Err(err) = send_result {
            warn!("failed to send request {outgoing_message_id:?} to client: {err:?}");
            let mut request_id_to_callback = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let no_remaining_recipient = request_id_to_callback
                .get_mut(&outgoing_message_id)
                .is_some_and(|entry| {
                    let delivered_connection_ids = &entry.delivered_connection_ids;
                    entry
                        .connection_ids
                        .retain(|connection_id| delivered_connection_ids.contains(connection_id));
                    entry.connection_ids.is_empty()
                });
            let undelivered_entry = no_remaining_recipient
                .then(|| request_id_to_callback.remove(&outgoing_message_id))
                .flatten();
            drop(request_id_to_callback);
            if let Some(entry) = undelivered_entry {
                let _ = entry.callback.send(Err(internal_error(
                    ALL_AUTHORIZED_CONNECTION_SENDS_FAILED_ERROR,
                )));
            }
        }
        pending_registration.disarm();
        Ok((outgoing_message_id, rx_approve))
    }

    pub(crate) async fn replay_requests_to_connection_for_thread(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
        _experimental_api_enabled: bool,
    ) {
        let (request_ids, target_closed) = {
            let active_connections = Arc::clone(&self.active_connections).lock_owned().await;
            let is_active = active_connections
                .get(&connection_id)
                .is_some_and(|connection| connection.initialized.load(Ordering::Acquire));
            if !is_active {
                return;
            }
            let request_id_to_callback = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let requests = request_id_to_callback
                .iter()
                .filter_map(|(request_id, entry)| {
                    if entry.thread_id != Some(thread_id) {
                        return None;
                    }
                    // Execution may already have produced effects. Subscription
                    // replay supplies no deduplication or result-reuse contract.
                    if matches!(entry.request, ServerRequest::DynamicToolCall { .. }) {
                        return None;
                    }
                    Some((request_id.clone(), entry.settlement.clone()))
                })
                .collect::<Vec<_>>();
            (requests, active_connections[&connection_id].delivery_failure.clone())
        };
        let delivery_deadline = tokio::time::Instant::now() + RESOURCE_DELIVERY_TIMEOUT;
        for (request_id, settlement) in request_ids {
            let permit = tokio::select! {
                biased;
                _ = target_closed.cancelled() => return,
                _ = settlement.cancelled() => continue,
                _ = self.delivery_shutdown.cancelled() => {
                    warn!("request replay was cancelled by shutdown");
                    return;
                }
                result = tokio::time::timeout_at(delivery_deadline, self.sender.reserve()) => {
                    match result {
                        Ok(Ok(permit)) => permit,
                        Ok(Err(err)) => {
                            warn!(%err, "failed to reserve capacity to resend request to client");
                            return;
                        }
                        Err(_) => {
                            warn!("request replay exceeded the resource delivery budget");
                            return;
                        }
                    }
                }
            };
            // Capacity admission can outlive a disconnect. Keep the active
            // connection and callback authority fenced through publication.
            let active_connections = Arc::clone(&self.active_connections).lock_owned().await;
            if !active_connections
                .get(&connection_id)
                .is_some_and(|connection| connection.initialized.load(Ordering::Acquire))
            {
                return;
            }
            let mut request_id_to_callback = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(entry) = request_id_to_callback.get_mut(&request_id) else {
                continue;
            };
            entry.connection_ids.insert(connection_id);
            entry.delivered_connection_ids.insert(connection_id);
            permit.send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::Request(entry.request.clone()),
                write_complete_tx: None,
            });
        }
    }

    pub(crate) async fn notify_client_response(
        &self,
        connection_id: ConnectionId,
        id: RequestId,
        result: Result,
    ) {
        let entry = self
            .take_request_callback_from_connection(connection_id, &id)
            .await;

        match entry {
            TakeCallbackResult::Taken(id, entry) => {
                let completed_at_ms = now_unix_timestamp_ms();
                self.analytics_events_client.track_server_response(
                    completed_at_ms,
                    &entry.request,
                    &result,
                );
                if let Err(err) = entry.callback.send(Ok(result)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            TakeCallbackResult::Unauthorized => {
                warn!(
                    "ignoring response for {id:?} from unauthorized connection {connection_id:?}"
                );
            }
            TakeCallbackResult::Missing => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn notify_client_error(
        &self,
        connection_id: ConnectionId,
        id: RequestId,
        error: JSONRPCErrorError,
    ) {
        let entry = self
            .take_request_callback_from_connection(connection_id, &id)
            .await;

        match entry {
            TakeCallbackResult::Taken(id, entry) => {
                warn!("client responded with error for {id:?}: {error:?}");
                self.analytics_events_client
                    .track_server_request_aborted(now_unix_timestamp_ms(), id.clone());
                if let Err(err) = entry.callback.send(Err(error)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            TakeCallbackResult::Unauthorized => {
                warn!("ignoring error for {id:?} from unauthorized connection {connection_id:?}");
            }
            TakeCallbackResult::Missing => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) fn cancel_request_sync(&self, id: &RequestId) -> bool {
        let entry = self.remove_request_callback_sync(id);
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
            let mut request_id_to_callback = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    async fn take_request_callback_from_connection(
        &self,
        connection_id: ConnectionId,
        id: &RequestId,
    ) -> TakeCallbackResult {
        let mut request_id_to_callback = self
            .request_id_to_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = request_id_to_callback.get(id) else {
            return TakeCallbackResult::Missing;
        };
        if !entry.connection_ids.contains(&connection_id) {
            return TakeCallbackResult::Unauthorized;
        }
        match request_id_to_callback.remove_entry(id) {
            Some((id, entry)) => TakeCallbackResult::Taken(id, Box::new(entry)),
            None => TakeCallbackResult::Missing,
        }
    }

    fn remove_request_callback_sync(
        &self,
        id: &RequestId,
    ) -> Option<(RequestId, PendingCallbackEntry)> {
        self.request_id_to_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove_entry(id)
    }

    #[cfg(test)]
    pub(crate) async fn pending_requests_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ServerRequest> {
        let request_id_to_callback = self
            .request_id_to_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        // Thread unload reaches this cleanup even when backpressure prevents publishing
        // ThreadClosed. Do not retain per-thread dedup state until connection shutdown.
        self.component_notification_cache
            .lock()
            .await
            .remove_thread(&thread_id.to_string());
        let entries = {
            let mut request_id_to_callback = self
                .request_id_to_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    /// Prepare a resume response before acquiring its listener-generation fence.
    /// The commit guard is retained through synchronous enqueue, so neither
    /// metadata contention nor transport backpressure pins the thread state.
    pub(crate) async fn send_resume_response_with_commit<G>(
        &self,
        request_id: ConnectionRequestId,
        response: codex_app_server_protocol::ThreadResumeResponse,
        thread_originator: String,
        commit: impl std::future::Future<Output = std::result::Result<Option<G>, JSONRPCErrorError>>,
    ) -> bool {
        let response = ClientResponsePayload::from(response);
        let serialized = response.into_jsonrpc_parts_and_payload(request_id.request_id.clone());
        let (id, result, analytics_response) = match serialized {
            Ok(parts) => parts,
            Err(error) => {
                self.send_error(
                    request_id,
                    internal_error(format!("failed to serialize response: {error}")),
                )
                .await;
                return false;
            }
        };
        let request_context = self.request_context(&request_id).await;
        let reserve = async {
            tokio::select! {
                biased;
                _ = self.delivery_shutdown.cancelled() => None,
                result = tokio::time::timeout(RESOURCE_DELIVERY_TIMEOUT, self.sender.reserve()) => {
                    match result {
                        Ok(Ok(permit)) => Some(permit),
                        Ok(Err(error)) => {
                            warn!("failed to reserve resume response capacity: {error:?}");
                            None
                        }
                        Err(_) => {
                            warn!(connection_id = ?request_id.connection_id,
                                "timed out enqueueing resume response");
                            None
                        }
                    }
                }
            }
        };
        let permit = if let Some(context) = request_context.as_ref() {
            reserve.instrument(context.span()).await
        } else {
            reserve.await
        };
        let Some(permit) = permit else {
            self.fail_connection_delivery(request_id.connection_id)
                .await;
            self.finish_request_context(request_context.as_ref()).await;
            return false;
        };
        let commit_result = tokio::select! {
            biased;
            _ = self.delivery_shutdown.cancelled() => {
                self.finish_request_context(request_context.as_ref()).await;
                return false;
            },
            result = commit => result,
        };
        let (message, guard) = match commit_result {
            Ok(Some(guard)) => {
                if let Some(response) = analytics_response {
                    self.analytics_events_client
                        .track_response_with_thread_originator(
                            request_id.connection_id.0,
                            request_id.request_id,
                            response,
                            thread_originator,
                        );
                }
                (
                    OutgoingMessage::Response(OutgoingResponse { id, result }),
                    Some(guard),
                )
            }
            Ok(None) => {
                self.finish_request_context(request_context.as_ref()).await;
                return false;
            }
            Err(error) => (OutgoingMessage::Error(OutgoingError { id, error }), None),
        };
        let committed = guard.is_some();
        permit.send(OutgoingEnvelope::ToConnection {
            connection_id: request_id.connection_id,
            message,
            write_complete_tx: None,
        });
        drop(guard);
        self.finish_request_context(request_context.as_ref()).await;
        committed
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
        let request_context = self.request_context(&request_id).await;

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

    pub(crate) fn try_send_server_notification(&self, notification: ServerNotification) -> bool {
        tracing::trace!("app-server event: {notification}");
        if let Err(err) = self
            .notification_sender()
            .try_send(OutgoingEnvelope::Broadcast {
                message: OutgoingMessage::AppServerNotification(notification),
            })
        {
            warn!("failed to enqueue server notification to client: {err:?}");
            return false;
        }
        true
    }

    /// Tries a resource-owned delta without blocking its pipe reader. The caller
    /// must retain rejected bytes for its terminal response instead of dropping them.
    pub(crate) fn try_send_server_notification_to_connection(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
        cancellation: &CancellationToken,
    ) -> bool {
        if cancellation.is_cancelled() || self.delivery_shutdown.is_cancelled() {
            return false;
        }
        self.notification_sender()
            .try_send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::AppServerNotification(notification),
                write_complete_tx: None,
            })
            .is_ok()
    }

    // Keep the timed fanout state out of callers that dispatch many notification variants.
    // Only allocation is eager; tracing, deadlines, locks and delivery still begin on poll.
    pub(crate) fn send_server_notification_to_connections<'a>(
        &'a self,
        connection_ids: &'a [ConnectionId],
        notification: ServerNotification,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            tracing::trace!(
                targeted_connections = connection_ids.len(),
                "app-server event: {notification}"
            );
            let outgoing_message = OutgoingMessage::AppServerNotification(notification);
            let delivery_deadline = tokio::time::Instant::now() + RESOURCE_DELIVERY_TIMEOUT;
            if connection_ids.is_empty() {
                let targets: Vec<_> = self
                    .active_connections
                    .lock()
                    .await
                    .keys()
                    .copied()
                    .collect();
                let sent = tokio::select! {
                    biased;
                    _ = self.delivery_shutdown.cancelled() => false,
                    result = tokio::time::timeout_at(delivery_deadline, self.notification_sender().send(OutgoingEnvelope::Broadcast {
                        message: outgoing_message,
                    })) => matches!(result, Ok(Ok(()))),
                };
                if !sent {
                    warn!("server notification delivery failed or exceeded its budget");
                    for target in targets {
                        self.fail_connection_delivery(target).await;
                    }
                }
                return;
            }
            let mut outgoing_message = Some(outgoing_message);
            for (index, connection_id) in connection_ids.iter().enumerate() {
                let Some(message) =
                    clone_or_take_last(&mut outgoing_message, index + 1 == connection_ids.len())
                else {
                    warn!("targeted notification value was exhausted before fanout completed");
                    break;
                };
                let sent = tokio::select! {
                    biased;
                    _ = self.delivery_shutdown.cancelled() => false,
                    result = tokio::time::timeout_at(delivery_deadline, self.notification_sender().send(OutgoingEnvelope::ToConnection {
                        connection_id: *connection_id,
                        message,
                        write_complete_tx: None,
                    })) => matches!(result, Ok(Ok(()))),
                };
                if !sent {
                    warn!(
                        ?connection_id,
                        "server notification delivery failed or exceeded its budget"
                    );
                    self.fail_connection_delivery(*connection_id).await;
                }
            }
        })
    }

    /// Enqueues one resource-owned notification without allowing transport
    /// backpressure to own the resource's lifetime.
    pub(crate) async fn send_server_notification_to_connection_bounded(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
        cancellation: &CancellationToken,
    ) -> bool {
        tracing::trace!(?connection_id, "app-server bounded event: {notification}");
        let send = self
            .notification_sender()
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::AppServerNotification(notification),
                write_complete_tx: None,
            });
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return false,
            _ = self.delivery_shutdown.cancelled() => return false,
            result = tokio::time::timeout(RESOURCE_DELIVERY_TIMEOUT, send) => result,
        };
        match result {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                warn!(
                    ?connection_id,
                    "failed to enqueue resource notification: {err:?}"
                );
                self.fail_connection_delivery(connection_id).await;
                false
            }
            Err(_) => {
                warn!(
                    ?connection_id,
                    timeout_ms = RESOURCE_DELIVERY_TIMEOUT.as_millis(),
                    "timed out enqueueing resource notification"
                );
                self.fail_connection_delivery(connection_id).await;
                false
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
        core_completed_at: Option<Instant>,
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = target_connection_ids.len(),
            "app-server terminal event: {notification}"
        );
        let dispatch_started = Instant::now();
        tracing::info!(target: "codex_task_observation", schema_version = 1,
            %thread_id, %turn_id, phase = "server_notification_dispatch_started",
            at_unix_ms = chrono::Utc::now().timestamp_millis(), clock = "unix_wall_ms",
            "task boundary observation");
        let delivery_deadline = tokio::time::Instant::now() + RESOURCE_DELIVERY_TIMEOUT;
        let receipt_deadline = tokio::time::Instant::now() + TURN_DELIVERY_RECEIPT_TIMEOUT;
        let outgoing_message = OutgoingMessage::AppServerNotification(notification);
        let mut receipts = Vec::with_capacity(target_connection_ids.len());

        for connection_id in target_connection_ids {
            let (write_complete_tx, write_complete_rx) = oneshot::channel();
            let (filtered_tx, filtered_rx) = oneshot::channel();
            let send = self
                .notification_sender()
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: outgoing_message.clone(),
                    write_complete_tx: Some(OutgoingWriteReceipt {
                        writer: write_complete_tx,
                        filtered: Some(filtered_tx),
                    }),
                });
            let send_result = tokio::select! {
                biased;
                _ = self.delivery_shutdown.cancelled() => None,
                result = tokio::time::timeout_at(delivery_deadline, send) => Some(result),
            };
            let immediate_outcome = turn_enqueue_outcome(&send_result);
            if immediate_outcome.is_some() {
                warn!(
                    "failed to dispatch terminal notification to {connection_id:?}: {send_result:?}"
                );
                self.fail_connection_delivery(connection_id).await;
            }
            receipts.push(PendingTurnDeliveryReceipt {
                connection_id,
                receiver: immediate_outcome.is_none().then_some(write_complete_rx),
                filtered: immediate_outcome.is_none().then_some(filtered_rx),
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
                    core_completed_at,
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

    /// The host owns the shared deadline and force signal. Do not start an
    /// earlier independent grace that discards a healthy pending write receipt.
    pub(crate) async fn settle_delivery_tasks(&self) {
        {
            let mut accepting = self.delivery_accepting.lock().await;
            *accepting = false;
            self.delivery_tasks.close();
        }
        self.delivery_tasks.wait().await;
    }

    /// Bounded test teardown that force-cancels stragglers. Production settles
    /// through [`Self::settle_delivery_tasks`] under the host's deadline.
    #[cfg(test)]
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
            let _ = tokio::time::timeout(TURN_DELIVERY_SHUTDOWN_GRACE, self.delivery_tasks.wait())
                .await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn send_server_notification_to_connection_and_wait(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
    ) {
        tracing::trace!("app-server event: {notification}");
        let outgoing_message = OutgoingMessage::AppServerNotification(notification.clone());
        let (write_complete_tx, write_complete_rx) = oneshot::channel();
        let send = self
            .notification_sender()
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: outgoing_message,
                write_complete_tx: Some(write_complete_tx.into()),
            });
        match tokio::select! {
            biased;
            _ = self.delivery_shutdown.cancelled() => return,
            result = tokio::time::timeout(RESOURCE_DELIVERY_TIMEOUT, send) => result,
        } {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!("failed to send server notification to client: {err:?}");
                return;
            }
            Err(_) => {
                warn!(
                    ?connection_id,
                    timeout_ms = RESOURCE_DELIVERY_TIMEOUT.as_millis(),
                    "timed out enqueueing acknowledged server notification"
                );
                return;
            }
        }
        match tokio::select! {
            biased;
            _ = self.delivery_shutdown.cancelled() => return,
            result = tokio::time::timeout(WRITER_ACKNOWLEDGEMENT_TIMEOUT, write_complete_rx) => result,
        } {
            Ok(Ok(_delivered_at)) => {}
            Ok(Err(err)) => {
                warn!(
                    ?connection_id,
                    "server notification writer acknowledgement dropped: {err}"
                )
            }
            Err(_) => warn!(
                ?connection_id,
                timeout_ms = WRITER_ACKNOWLEDGEMENT_TIMEOUT.as_millis(),
                "timed out waiting for server notification writer acknowledgement"
            ),
        }
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: impl Into<JSONRPCErrorError>,
    ) {
        let request_context = self.request_context(&request_id).await;
        self.send_error_inner(request_context, request_id, error.into())
            .await;
    }

    /// Rejections retain their ID and lease without occupying shared ingress.
    /// Exhaustion fails the route instead of creating unbounded delivery tasks.
    pub(crate) async fn queue_error(
        self: &Arc<Self>,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        let context = self.request_context(&request_id).await;
        let Ok(slot) = Arc::clone(&self.rejection_slots).try_acquire_owned() else {
            self.fail_connection_delivery(request_id.connection_id).await;
            self.finish_request_context(context.as_ref()).await;
            return;
        };
        let accepting = self.delivery_accepting.lock().await;
        if !*accepting {
            self.fail_connection_delivery(request_id.connection_id).await;
            self.finish_request_context(context.as_ref()).await;
            return;
        }
        let route_closed = self.active_connections.lock().await
            .get(&request_id.connection_id).map(|connection| connection.delivery_failure.clone())
            .unwrap_or_default();
        let outgoing = Arc::clone(self);
        self.delivery_tasks.spawn(async move {
            let _slot = slot;
            // Keep registration until terminal admission/failure, so a second
            // envelope cannot claim this ID while delivery is still pending.
            tokio::select! {
                biased;
                _ = route_closed.cancelled() => {}
                _ = outgoing.send_error_inner(context.clone(), request_id, error) => {}
            }
            outgoing.finish_request_context(context.as_ref()).await;
        });
    }

    /// Claim a rejected envelope too: otherwise an oversized duplicate could
    /// publish a same-ID error while the original request still owns that ID.
    pub(crate) async fn send_unregistered_error(
        self: &Arc<Self>,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        let context = RequestContext::new(request_id.clone(), tracing::Span::none(), None);
        if self.register_request_context(context).await {
            self.queue_error(request_id, error).await;
        } else {
            self.fail_connection_delivery(request_id.connection_id)
                .await;
        }
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
        let request_span = request_context.as_ref().map(RequestContext::span);
        if let Some(span) = request_span.as_ref() {
            match &message {
                OutgoingMessage::Response(_) => {
                    span.record("app_server.outcome", "success");
                }
                OutgoingMessage::Error(error) => {
                    span.record("app_server.outcome", "error");
                    span.record("rpc.jsonrpc.error_code", error.error.code);
                    span.record("error.type", "jsonrpc");
                    span.record("otel.status_code", "ERROR");
                }
                _ => {}
            }
        }
        let route_failed = self.active_connections.lock().await
            .get(&connection_id).map(|connection| connection.delivery_failure.clone())
            .unwrap_or_default();
        let send_fut = self.sender.send(OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx: None,
        });
        let send_fut = async {
            tokio::select! {
                biased;
                _ = route_failed.cancelled() => None,
                _ = self.delivery_shutdown.cancelled() => None,
                result = tokio::time::timeout(RESOURCE_DELIVERY_TIMEOUT, send_fut) => Some(result),
            }
        };
        let send_result = if let Some(request_context) = request_context.as_ref() {
            send_fut.instrument(request_context.span()).await
        } else {
            send_fut.await
        };

        if let Some(span) = request_span.as_ref() {
            span.record(
                "app_server.delivery_outcome",
                match &send_result {
                    Some(Ok(Ok(()))) => "enqueued",
                    Some(Ok(Err(_))) => "closed",
                    Some(Err(_)) => "timeout",
                    None => "cancelled",
                },
            );
        }

        if !matches!(&send_result, Some(Ok(Ok(())))) {
            self.fail_connection_delivery(connection_id).await;
        }
        // Keep the claim through admission or route failure. Cleanup from an
        // older delivery must never erase a same-ID successor after teardown.
        self.finish_request_context(request_context.as_ref()).await;
        match send_result {
            Some(Ok(Ok(()))) => {}
            Some(Ok(Err(err))) => warn!("failed to send {message_kind} to client: {err:?}"),
            Some(Err(_)) => warn!(
                ?connection_id,
                timeout_ms = RESOURCE_DELIVERY_TIMEOUT.as_millis(),
                "timed out enqueueing {message_kind}"
            ),
            None => {}
        }
    }
}

fn turn_enqueue_outcome(
    result: &Option<
        std::result::Result<std::result::Result<(), mpsc::error::SendError<OutgoingEnvelope>>, tokio::time::error::Elapsed>,
    >,
) -> Option<TurnDeliveryOutcomeKind> {
    match result {
        Some(Ok(Ok(()))) => None,
        None => Some(TurnDeliveryOutcomeKind::ShutdownCancelled),
        Some(Err(_)) => Some(TurnDeliveryOutcomeKind::Timeout),
        Some(Ok(Err(_))) => Some(TurnDeliveryOutcomeKind::Failure),
    }
}

async fn collect_turn_delivery_outcomes(
    receipts: Vec<PendingTurnDeliveryReceipt>,
    dispatch_started: Instant,
    receipt_deadline: tokio::time::Instant,
    core_completed_at: Option<Instant>,
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
            // A filter acknowledgement is distinct from a writer timestamp.
            // Dropping the optional filter sender on the ordinary write path
            // must not turn a still-pending physical write into a failure.
            let receiver = async move {
                let mut receiver = receiver;
                if let Some(filtered) = receipt.filtered {
                    tokio::select! {
                        biased;
                        result = filtered => {
                            if result.is_ok() {
                                return Ok(None);
                            }
                        }
                        result = &mut receiver => return result.map(Some),
                    }
                }
                receiver.await.map(Some)
            };
            let receipt_result = tokio::select! {
                biased;
                result = tokio::time::timeout_at(receipt_deadline, receiver) => Some(result),
                _ = delivery_shutdown.cancelled() => None,
            };
            match receipt_result {
                Some(Ok(Ok(Some(delivered_at)))) => {
                    let successful_elapsed_ms = u64::try_from(
                        delivered_at
                            .saturating_duration_since(dispatch_started)
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX);
                    TurnDeliveryOutcome {
                        connection_id: receipt.connection_id,
                        kind: TurnDeliveryOutcomeKind::Success,
                        successful_elapsed_ms: Some(successful_elapsed_ms),
                        post_core_delivery_latency_ms: core_completed_at
                            .and_then(|completed_at| {
                                delivered_at.checked_duration_since(completed_at)
                            })
                            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
                    }
                }
                Some(Ok(Ok(None))) => TurnDeliveryOutcome {
                    connection_id: receipt.connection_id,
                    kind: TurnDeliveryOutcomeKind::Filtered,
                    successful_elapsed_ms: None,
                    post_core_delivery_latency_ms: None,
                },
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
        Some(TurnDeliveryOutcomeKind::Filtered) => TurnDeliveryStatus::Filtered,
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
    let filtered_count = outcomes
        .iter()
        .filter(|outcome| outcome.kind == TurnDeliveryOutcomeKind::Filtered)
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

    // These are transport-writer receipts, NOT acknowledgements from the
    // client or an independent correctness oracle. Success timestamps are
    // dispatch-relative, not the later receipt-collection wall time.
    tracing::info!(target: "codex_task_observation", schema_version = 1,
        %thread_id, %turn_id, phase = "server_notification_receipts_collected",
        endpoint = "transport_writer_flush", client_visible = false,
        origin_status = ?origin_delivery_status,
        origin_dispatch_elapsed_ms = origin_outcome.and_then(|outcome| outcome.successful_elapsed_ms),
        first_dispatch_elapsed_ms = first_successful_elapsed_ms,
        last_dispatch_elapsed_ms = last_successful_elapsed_ms,
        success_count, failure_count, timeout_count, filtered_count,
        "task boundary observation");

    TurnDeliveryFact {
        thread_id: thread_id.to_string(),
        turn_id,
        target_count: count_u32(outcomes.len()),
        success_count: count_u32(success_count),
        filtered_count: count_u32(filtered_count),
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

fn clone_or_take_last<T: Clone>(value: &mut Option<T>, is_last: bool) -> Option<T> {
    if is_last {
        value.take()
    } else {
        value.as_ref().cloned()
    }
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
    #[tokio::test]
    async fn rejection_delivery_is_bounded_and_keeps_identity_until_settlement() {
        for disconnect in [false, true] {
            let (tx, mut rx) = mpsc::channel(1);
            let held_capacity = tx.clone().reserve_owned().await.unwrap();
            let outgoing = Arc::new(OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled()));
            let connection_id = ConnectionId(99);
            outgoing.connection_opened(connection_id, Arc::new(AtomicBool::new(true))).await;
            let request_id = ConnectionRequestId { connection_id, request_id: RequestId::Integer(7) };
            let context = || RequestContext::new(request_id.clone(), Span::none(), None);
            assert!(outgoing.register_request_context(context()).await);
            let admission = outgoing.queue_error(request_id.clone(), internal_error("rejected"));
            tokio::pin!(admission);
            assert!(futures::poll!(&mut admission).is_ready(), "full output must not hold ingress");
            tokio::task::yield_now().await;
            assert!(!outgoing.register_request_context(context()).await);
            assert_eq!(outgoing.rejection_slots.available_permits(), 63);
            if disconnect {
                outgoing.connection_closed(connection_id).await;
                timeout(Duration::from_secs(1), outgoing.settle_delivery_tasks()).await
                    .expect("disconnect retires rejection while output remains full");
                drop(held_capacity);
                assert!(rx.try_recv().is_err());
            } else {
                drop(held_capacity);
                let Some(OutgoingEnvelope::ToConnection { message: OutgoingMessage::Error(error), .. }) = rx.recv().await else { panic!("correlated error"); };
                assert_eq!(error.id, request_id.request_id);
                outgoing.settle_delivery_tasks().await;
                assert!(outgoing.register_request_context(context()).await);
            }
            assert_eq!(outgoing.rejection_slots.available_permits(), 64);
        }
    }

    #[tokio::test]
    async fn rejection_budget_exhaustion_fails_the_route_without_spawning() {
        let (tx, _rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled()));
        let connection_id = ConnectionId(99);
        let initialized = Arc::new(AtomicBool::new(true));
        outgoing.connection_opened(connection_id, Arc::clone(&initialized)).await;
        let _slots = Arc::clone(&outgoing.rejection_slots).acquire_many_owned(64).await.unwrap();
        outgoing.send_unregistered_error(ConnectionRequestId { connection_id, request_id: RequestId::Integer(7) }, internal_error("rejected")).await;
        assert!(!initialized.load(Ordering::Acquire));
        assert_eq!(outgoing.request_context_count().await, 0);
        assert_eq!(outgoing.delivery_tasks.len(), 0);
    }

    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use codex_app_server_protocol::AccountLoginCompletedNotification;
    use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::ApplyPatchApprovalParams;
    use codex_app_server_protocol::AttestationGenerateParams;
    use codex_app_server_protocol::AuthMode;
    use codex_app_server_protocol::CommandExecutionApprovalDecision;
    use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
    use codex_app_server_protocol::ConfigWarningNotification;
    use codex_app_server_protocol::DynamicToolCallParams;
    use codex_app_server_protocol::ExecCommandApprovalParams;
    use codex_app_server_protocol::FileChangeRequestApprovalParams;
    use codex_app_server_protocol::ModelRerouteReason;
    use codex_app_server_protocol::ModelReroutedNotification;
    use codex_app_server_protocol::ModelVerification;
    use codex_app_server_protocol::ModelVerificationNotification;
    use codex_app_server_protocol::RateLimitSnapshot;
    use codex_app_server_protocol::RateLimitWindow;
    use codex_app_server_protocol::ServerResponse;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnItemsView;
    use codex_app_server_protocol::TurnModerationMetadataNotification;
    use codex_app_server_protocol::TurnStatus;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;

    #[derive(Debug)]
    struct CloneCounter(Arc<AtomicUsize>);

    impl Clone for CloneCounter {
        fn clone(&self) -> Self {
            self.0.fetch_add(1, Ordering::SeqCst);
            Self(Arc::clone(&self.0))
        }
    }

    #[test]
    fn targeted_notification_moves_the_last_value_instead_of_cloning_it() {
        let clone_count = Arc::new(AtomicUsize::new(0));
        let mut single = Some(CloneCounter(Arc::clone(&clone_count)));
        assert!(clone_or_take_last(&mut single, true).is_some());
        assert_eq!(clone_count.load(Ordering::SeqCst), 0);

        let mut multiple = Some(CloneCounter(Arc::clone(&clone_count)));
        assert!(clone_or_take_last(&mut multiple, false).is_some());
        assert!(clone_or_take_last(&mut multiple, true).is_some());
        assert_eq!(clone_count.load(Ordering::SeqCst), 1);
        assert!(
            clone_or_take_last(&mut multiple, true).is_none(),
            "an exhausted fanout value must not panic"
        );
    }

    fn turn_completed_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            surfaced_result: None,
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
                timing: None,
                surfaced_result: None,
            },
            timing: None,
        })
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
                    spend_control_reached: None,
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
                        "spendControlReached": null,
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
    async fn terminal_backpressure_retains_request_identity_and_work_lease() {
        for error in [false, true] {
            for control in [false, true] {
                let (tx, mut rx) = mpsc::channel(1);
                let held_capacity = tx.clone().reserve_owned().await.unwrap();
                let outgoing = OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled());
                let connection_id = ConnectionId(42);
                outgoing.connection_opened(connection_id, Arc::new(AtomicBool::new(true))).await;
                let request_id = ConnectionRequestId { connection_id, request_id: RequestId::Integer(7) };
                let mut context = RequestContext::new(request_id.clone(), Span::none(), None);
                assert!(outgoing.register_request_context(context.clone()).await);
                assert!(outgoing.admit_request_work(&mut context, 100, control).await);
                drop(context);
                let mut terminal = Box::pin(async {
                    if error {
                        outgoing.send_error(request_id.clone(), internal_error("failed")).await;
                    } else {
                        outgoing.send_response(request_id.clone(), codex_app_server_protocol::ThreadArchiveResponse {}).await;
                    }
                });
                assert!(futures::poll!(terminal.as_mut()).is_pending());
                assert_eq!(outgoing.request_context_count().await, 1);
                assert!(!outgoing.register_request_context(RequestContext::new(request_id.clone(), Span::none(), None)).await);
                assert_eq!(outgoing.outstanding.available_permits(), if control { 1024 } else { 1023 });
                assert_eq!(outgoing.outstanding_control.available_permits(), if control { 63 } else { 64 });
                drop(held_capacity);
                terminal.await;
                let OutgoingEnvelope::ToConnection { connection_id: delivered_to, message, .. } = rx.try_recv().unwrap() else {
                    panic!("terminal must stay targeted");
                };
                assert_eq!(delivered_to, connection_id);
                match message {
                    OutgoingMessage::Response(response) => {
                        assert!(!error);
                        assert_eq!(response.id, request_id.request_id);
                    }
                    OutgoingMessage::Error(response) => {
                        assert!(error);
                        assert_eq!(response.id, request_id.request_id);
                    }
                    _ => panic!("expected terminal response or error"),
                }
                assert_eq!(outgoing.request_context_count().await, 0);
                assert_eq!(outgoing.outstanding.available_permits(), 1024);
                assert_eq!(outgoing.outstanding_control.available_permits(), 64);
                assert!(outgoing.register_request_context(RequestContext::new(request_id, Span::none(), None)).await);
            }
        }
    }

    #[tokio::test]
    async fn failed_terminal_admission_releases_claim_and_lease() {
        for shutdown in [false, true] {
            let (tx, rx) = mpsc::channel(1);
            let held_capacity = tx.clone().reserve_owned().await.unwrap();
            let outgoing = OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled());
            let connection_id = ConnectionId(42);
            let failure = CancellationToken::new();
            outgoing.connection_opened_with_runtime(connection_id, Arc::new(AtomicBool::new(true)), failure.clone()).await;
            let request_id = ConnectionRequestId { connection_id, request_id: RequestId::Integer(7) };
            let mut context = RequestContext::new(request_id.clone(), Span::none(), None);
            assert!(outgoing.register_request_context(context.clone()).await);
            assert!(outgoing.admit_request_work(&mut context, 100, false).await);
            drop(context);
            let mut terminal = Box::pin(outgoing.send_error(request_id, internal_error("failed")));
            assert!(futures::poll!(terminal.as_mut()).is_pending());
            if shutdown {
                outgoing.delivery_shutdown.cancel();
            } else {
                drop(rx);
            }
            terminal.await;
            assert!(failure.is_cancelled());
            assert_eq!(outgoing.request_context_count().await, 0);
            assert_eq!(outgoing.outstanding.available_permits(), 1024);
            drop(held_capacity);
        }
    }

    #[tokio::test]
    async fn old_terminal_cleanup_cannot_erase_a_same_id_successor() {
        let (tx, mut rx) = mpsc::channel(1);
        let held_capacity = tx.clone().reserve_owned().await.unwrap();
        let outgoing = OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled());
        let connection_id = ConnectionId(42);
        outgoing.connection_opened(connection_id, Arc::new(AtomicBool::new(true))).await;
        let request_id = ConnectionRequestId { connection_id, request_id: RequestId::Integer(7) };
        let mut original = RequestContext::new(request_id.clone(), Span::none(), None);
        assert!(outgoing.register_request_context(original.clone()).await);
        let mut terminal = Box::pin(outgoing.send_response(request_id.clone(), codex_app_server_protocol::ThreadArchiveResponse {}));
        assert!(futures::poll!(terminal.as_mut()).is_pending());
        let mut contexts = outgoing.request_contexts.lock().await;
        drop(held_capacity);
        assert!(futures::poll!(terminal.as_mut()).is_pending(), "hold cleanup after synchronous enqueue");
        // Inject the teardown/reuse cutoff while the old cleanup is suspended.
        let successor = RequestContext::new(request_id.clone(), Span::none(), None);
        contexts.insert(request_id.clone(), successor.clone());
        drop(contexts);
        terminal.await;
        assert!(rx.try_recv().is_ok());
        let current = outgoing.request_context(&request_id).await.unwrap();
        assert!(Arc::ptr_eq(&current.generation, &successor.generation));
        assert!(!outgoing.admit_request_work(&mut original, 100, false).await, "late admission cannot replace the successor either");
        assert_eq!(outgoing.request_context_count().await, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn control_response_deadline_retires_its_request_claim() {
        let (tx, _rx) = mpsc::channel(1);
        let held_capacity = tx.clone().reserve_owned().await.unwrap();
        let outgoing = Arc::new(OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled()));
        let connection_id = ConnectionId(42);
        let failure = CancellationToken::new();
        outgoing.connection_opened_with_runtime(connection_id, Arc::new(AtomicBool::new(true)), failure.clone()).await;
        let request_id = ConnectionRequestId { connection_id, request_id: RequestId::Integer(7) };
        let mut context = RequestContext::new(request_id.clone(), Span::none(), None);
        assert!(outgoing.register_request_context(context.clone()).await);
        assert!(outgoing.admit_request_work(&mut context, 0, true).await);
        drop(context);
        let scoped = ThreadScopedOutgoingMessageSender::new(outgoing.clone(), vec![connection_id], ThreadId::new());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut terminal = Box::pin(scoped.send_response_before(request_id.clone(), codex_app_server_protocol::TurnInterruptResponse {}, deadline));
        assert!(futures::poll!(terminal.as_mut()).is_pending());
        assert!(!outgoing.register_request_context(RequestContext::new(request_id, Span::none(), None)).await);
        tokio::time::advance(Duration::from_secs(1)).await;
        terminal.await;
        assert!(failure.is_cancelled());
        assert_eq!(outgoing.request_context_count().await, 0);
        assert_eq!(outgoing.outstanding_control.available_permits(), 64);
        drop(held_capacity);
    }

    #[tokio::test]
    async fn resume_keeps_request_claim_through_capacity_and_commit() {
        for disposition in 0..4 {
            let (tx, mut rx) = mpsc::channel(1);
            let held_capacity = tx.clone().reserve_owned().await.unwrap();
            let outgoing = OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled());
            let connection_id = ConnectionId(42);
            outgoing.connection_opened(connection_id, Arc::new(AtomicBool::new(true))).await;
            let request_id = ConnectionRequestId { connection_id, request_id: RequestId::Integer(7) };
            let context = || RequestContext::new(request_id.clone(), Span::none(), None);
            assert!(outgoing.register_request_context(context()).await);
            let cwd = std::env::current_dir().unwrap();
            let response = serde_json::from_value(json!({
                "thread": {
                    "id": ThreadId::new().to_string(), "sessionId": "session", "preview": "",
                    "ephemeral": false, "modelProvider": "test", "createdAt": 0, "updatedAt": 0,
                    "status": {"type": "idle"}, "cwd": cwd, "cliVersion": "test", "source": "cli", "turns": []
                },
                "model": "test", "modelProvider": "test", "cwd": cwd,
                "approvalPolicy": "never", "sandbox": {"type": "readOnly", "networkAccess": false}
            })).unwrap();
            let (commit_tx, commit_rx) = oneshot::channel();
            let mut terminal = Box::pin(outgoing.send_resume_response_with_commit(
                request_id.clone(), response, "test".into(), async { commit_rx.await.unwrap() },
            ));
            assert!(futures::poll!(terminal.as_mut()).is_pending());
            assert!(!outgoing.register_request_context(context()).await);
            drop(held_capacity);
            assert!(futures::poll!(terminal.as_mut()).is_pending(), "commit remains independently blocked");
            assert!(!outgoing.register_request_context(context()).await);
            if disposition == 3 {
                outgoing.delivery_shutdown.cancel();
            } else {
                let result = match disposition {
                    0 => Ok(Some(())),
                    1 => Ok(None),
                    _ => Err(internal_error("commit failed")),
                };
                commit_tx.send(result).unwrap();
            }
            assert_eq!(terminal.await, disposition == 0);
            assert_eq!(outgoing.request_context_count().await, 0);
            if disposition == 0 || disposition == 2 {
                let OutgoingEnvelope::ToConnection { message, .. } = rx.try_recv().unwrap() else { panic!("targeted resume terminal"); };
                assert_eq!(matches!(message, OutgoingMessage::Response(_)), disposition == 0);
            } else {
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn send_response_routes_to_target_and_clears_registered_context() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
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
                None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 1);
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
            let OutgoingEnvelope::ToConnection {
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
                .send(Instant::now())
                .expect("receipt collector should still be waiting");
        }
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "no duplicate dispatch is allowed"
        );
        outgoing.shutdown_delivery_tasks().await;
    }

    #[tokio::test]
    async fn terminal_dispatch_waits_for_temporary_queue_saturation() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        outgoing
            .send_server_notification(turn_completed_notification(thread_id, "previous"))
            .await;
        let scoped = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );
        let dispatch = scoped.send_server_notification_with_receipts(
            turn_completed_notification(thread_id, "current"),
            None,
        );
        tokio::pin!(dispatch);
        assert!(
            futures::poll!(&mut dispatch).is_pending(),
            "terminal delivery must wait for capacity"
        );
        rx.try_recv().expect("remove previous message");
        timeout(Duration::from_secs(1), dispatch)
            .await
            .expect("dispatch completes after capacity is available");
        let OutgoingEnvelope::ToConnection {
            message:
                OutgoingMessage::AppServerNotification(ServerNotification::TurnCompleted(notification)),
            write_complete_tx,
            ..
        } = rx.try_recv().expect("terminal notification was retained")
        else {
            panic!("expected terminal notification");
        };
        assert_eq!(notification.turn.id, "current");
        write_complete_tx
            .expect("receipt")
            .send(Instant::now())
            .expect("collector waiting");
        outgoing.shutdown_delivery_tasks().await;
    }

    #[tokio::test]
    async fn graceful_delivery_drain_waits_for_physical_receipt() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let scoped = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );
        scoped
            .send_server_notification_with_receipts(
                turn_completed_notification(thread_id, "last-turn"),
                None,
            )
            .await;
        let OutgoingEnvelope::ToConnection {
            write_complete_tx, ..
        } = rx.recv().await.unwrap()
        else {
            panic!("expected targeted terminal");
        };
        let mut drain = Box::pin(outgoing.settle_delivery_tasks());
        assert!(
            futures::poll!(drain.as_mut()).is_pending(),
            "queue admission is not physical write settlement"
        );
        write_complete_tx.unwrap().send(Instant::now()).unwrap();
        timeout(Duration::from_secs(1), drain).await.unwrap();
    }

    fn component_publication_cases(thread_id: ThreadId) -> Vec<(ServerNotification, ServerNotification)> {
        let usage = |tokens| {
            let usage = json!({"totalTokens": tokens, "inputTokens": tokens, "cachedInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0});
            ServerNotification::ThreadTokenUsageUpdated(serde_json::from_value(json!({
                "threadId": thread_id.to_string(), "turnId": "turn",
                "tokenUsage": {"total": usage, "last": usage, "modelContextWindow": null}
            })).unwrap())
        };
        let rates = |used_percent| ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
            rate_limits: RateLimitSnapshot {
                limit_id: Some("codex".into()), limit_name: None,
                primary: Some(RateLimitWindow { used_percent, window_duration_mins: Some(15), resets_at: Some(123) }),
                secondary: None, credits: None, individual_limit: None, spend_control_reached: None,
                plan_type: Some(PlanType::Plus), rate_limit_reached_type: None,
            },
        });
        vec![(usage(1), usage(2)), (rates(10), rates(20))]
    }

    #[tokio::test]
    async fn newer_component_publication_supersedes_stalled_fanout() {
        let thread_id = ThreadId::new();
        for (older, newer) in component_publication_cases(thread_id) {
            let (tx, mut rx) = mpsc::channel(1);
            let held_capacity = tx.clone().reserve_owned().await.unwrap();
            let outgoing = Arc::new(OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled()));
            let both = ThreadScopedOutgoingMessageSender::new(outgoing.clone(), vec![ConnectionId(1), ConnectionId(2)], thread_id);
            let second = ThreadScopedOutgoingMessageSender::new(outgoing.clone(), vec![ConnectionId(2)], thread_id);
            let mut older_send = Box::pin(both.send_component_notification_if_changed(older.clone()));
            assert!(futures::poll!(older_send.as_mut()).is_pending());
            let mut newer_send = Box::pin(second.send_component_notification_if_changed(newer.clone()));
            assert!(futures::poll!(newer_send.as_mut()).is_pending());
            drop(held_capacity);
            let older_done = futures::poll!(older_send.as_mut()).is_ready();
            let OutgoingEnvelope::ToConnection { connection_id, message: OutgoingMessage::AppServerNotification(delivered), .. } = rx.try_recv().unwrap() else { panic!("first recipient"); };
            assert_eq!(connection_id, ConnectionId(1));
            assert_eq!(serde_json::to_value(delivered).unwrap(), serde_json::to_value(older).unwrap());
            newer_send.await;
            let OutgoingEnvelope::ToConnection { connection_id, message: OutgoingMessage::AppServerNotification(delivered), .. } = rx.try_recv().unwrap() else { panic!("second recipient"); };
            assert_eq!(connection_id, ConnectionId(2));
            assert_eq!(serde_json::to_value(delivered).unwrap(), serde_json::to_value(&newer).unwrap());
            if !older_done {
                older_send.await;
            }
            assert!(rx.try_recv().is_err(), "older fanout must not overwrite c2's newer publication");
            second.send_component_notification_if_changed(newer).await;
            assert!(rx.try_recv().is_err(), "dedup must describe the actual last enqueue");
        }
    }

    #[tokio::test]
    async fn cancelled_component_admission_does_not_prime_dedup() {
        let thread_id = ThreadId::new();
        for (notification, _) in component_publication_cases(thread_id) {
            let (tx, mut rx) = mpsc::channel(1);
            let held_capacity = tx.clone().reserve_owned().await.unwrap();
            let outgoing = Arc::new(OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled()));
            let scoped = ThreadScopedOutgoingMessageSender::new(outgoing.clone(), vec![ConnectionId(1)], thread_id);
            let mut initial = Box::pin(outgoing.send_initial_component_notification_to_connection(ConnectionId(1), notification.clone()));
            assert!(futures::poll!(initial.as_mut()).is_pending());
            drop(initial);
            drop(held_capacity);
            scoped.send_component_notification_if_changed(notification).await;
            assert!(matches!(rx.try_recv(), Ok(OutgoingEnvelope::ToConnection { connection_id: ConnectionId(1), .. })), "cancelled replay must not suppress the first actual publication");
        }
    }

    #[tokio::test]
    async fn identical_live_component_does_not_cancel_forced_replay() {
        let thread_id = ThreadId::new();
        for (notification, _) in component_publication_cases(thread_id) {
            let (tx, mut rx) = mpsc::channel(1);
            let outgoing = Arc::new(OutgoingMessageSender::new(tx.clone(), AnalyticsEventsClient::disabled()));
            let scoped = ThreadScopedOutgoingMessageSender::new(outgoing.clone(), vec![ConnectionId(1)], thread_id);
            scoped.send_component_notification_if_changed(notification.clone()).await;
            rx.try_recv().unwrap();
            let held_capacity = tx.reserve_owned().await.unwrap();
            let mut replay = Box::pin(outgoing.send_initial_component_notification_to_connection(ConnectionId(1), notification.clone()));
            assert!(futures::poll!(replay.as_mut()).is_pending());
            scoped.send_component_notification_if_changed(notification).await;
            drop(held_capacity);
            replay.await;
            assert!(matches!(rx.try_recv(), Ok(OutgoingEnvelope::ToConnection { connection_id: ConnectionId(1), .. })), "explicit replay remains forced despite an identical live update");
        }
    }

    #[tokio::test]
    async fn token_usage_retains_only_latest_turn_and_delivers_each_turn() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let scoped = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );
        for index in 0..20 {
            let turn_id = format!("turn-{index}");
            let usage = json!({"totalTokens": 1, "inputTokens": 1, "cachedInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0});
            let notification = ServerNotification::ThreadTokenUsageUpdated(
                serde_json::from_value(json!({
                    "threadId": thread_id.to_string(), "turnId": turn_id,
                    "tokenUsage": {"total": usage, "last": usage, "modelContextWindow": null}
                }))
                .expect("usage notification"),
            );
            scoped
                .send_component_notification_if_changed(notification.clone())
                .await;
            let OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::AppServerNotification(delivered),
                ..
            } = rx.try_recv().expect("new turn must be delivered")
            else {
                panic!("expected targeted usage");
            };
            assert_eq!(
                serde_json::to_value(delivered).unwrap(),
                serde_json::to_value(&notification).unwrap()
            );
            scoped
                .send_component_notification_if_changed(notification)
                .await;
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            assert_eq!(
                outgoing
                    .component_notification_cache
                    .lock()
                    .await
                    .token_usage
                    .len(),
                1
            );
        }
        outgoing.connection_closed(ConnectionId(1)).await;
        assert!(
            outgoing
                .component_notification_cache
                .lock()
                .await
                .token_usage
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn response_admission_timeout_fails_the_connection() {
        let (tx, _rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled());
        let failure = CancellationToken::new();
        outgoing
            .connection_opened_with_runtime(
                ConnectionId(1),
                Arc::new(AtomicBool::new(true)),
                failure.clone(),
            )
            .await;
        let thread_id = ThreadId::new();
        outgoing
            .send_server_notification(turn_completed_notification(thread_id, "fill"))
            .await;
        outgoing
            .send_response(
                ConnectionRequestId {
                    connection_id: ConnectionId(1),
                    request_id: RequestId::Integer(1),
                },
                codex_app_server_protocol::TurnStartResponse {
                    turn: serde_json::from_value(
                        json!({"id":"turn-1", "items":[], "status":"completed"}),
                    )
                    .expect("valid turn"),
                },
            )
            .await;
        assert!(
            failure.is_cancelled(),
            "pending requests must be settled by connection shutdown"
        );
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
    async fn turn_delivery_filter_receipt_is_not_a_writer_failure_or_success() {
        use crate::transport::OutboundConnectionState;
        use crate::transport::OutboundNotificationOptOuts;
        use crate::transport::route_outgoing_envelope;

        let connection_id = ConnectionId(7);
        for (opted_out, connected, expected) in [
            (true, true, TurnDeliveryOutcomeKind::Filtered),
            (false, true, TurnDeliveryOutcomeKind::Success),
            (false, false, TurnDeliveryOutcomeKind::Failure),
        ] {
            let (writer, mut reader) = mpsc::channel(1);
            let methods = if opted_out {
                std::collections::HashSet::from(["turn/completed".to_string()])
            } else {
                std::collections::HashSet::new()
            };
            let mut connections = std::collections::HashMap::new();
            if connected {
                connections.insert(
                    connection_id,
                    OutboundConnectionState::new(
                        writer,
                        Arc::new(AtomicBool::new(true)),
                        Arc::new(AtomicBool::new(true)),
                        Arc::new(OutboundNotificationOptOuts::new(methods)),
                        None,
                        CancellationToken::new(),
                    ),
                );
            }
            let (written_tx, written_rx) = oneshot::channel();
            let (filtered_tx, filtered_rx) = oneshot::channel();
            let started = Instant::now();
            route_outgoing_envelope(
                &mut connections,
                OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::AppServerNotification(turn_completed_notification(
                        ThreadId::new(),
                        "filtered-turn",
                    )),
                    write_complete_tx: Some(OutgoingWriteReceipt {
                        writer: written_tx,
                        filtered: Some(filtered_tx),
                    }),
                },
            )
            .await;
            if expected == TurnDeliveryOutcomeKind::Success {
                let queued = timeout(Duration::from_secs(1), reader.recv())
                    .await
                    .unwrap()
                    .unwrap();
                queued
                    .write_complete_tx
                    .unwrap()
                    .send(Instant::now())
                    .unwrap();
            } else {
                assert!(
                    reader.try_recv().is_err(),
                    "filtered/disconnected output must not be written"
                );
            }
            let outcomes = collect_turn_delivery_outcomes(
                vec![PendingTurnDeliveryReceipt {
                    connection_id,
                    receiver: Some(written_rx),
                    filtered: Some(filtered_rx),
                    immediate_outcome: None,
                }],
                started,
                tokio::time::Instant::now() + Duration::from_secs(1),
                Some(started),
                CancellationToken::new(),
            )
            .await;
            assert_eq!(outcomes[0].kind, expected);
            let fact = aggregate_turn_delivery(
                ThreadId::new(),
                "turn".into(),
                Some(connection_id),
                outcomes,
            );
            assert_eq!(fact.filtered_count, u32::from(opted_out));
            assert_eq!(fact.failure_count, u32::from(!connected));
            assert_eq!(fact.success_count, u32::from(connected && !opted_out));
            if opted_out {
                assert_eq!(fact.origin_delivery_status, TurnDeliveryStatus::Filtered);
                assert_eq!(fact.origin_successful_elapsed_ms, None);
                assert_eq!(fact.first_post_core_delivery_latency_ms, None);
            }
        }
    }

    #[tokio::test]
    async fn receipt_filter_channel_closure_still_waits_for_physical_writer() {
        let (written_tx, written_rx) = oneshot::channel();
        let (filtered_tx, filtered_rx) = oneshot::channel();
        drop(filtered_tx);
        let shutdown = CancellationToken::new();
        let collection = collect_turn_delivery_outcomes(
            vec![PendingTurnDeliveryReceipt {
                connection_id: ConnectionId(1),
                receiver: Some(written_rx),
                filtered: Some(filtered_rx),
                immediate_outcome: None,
            }],
            Instant::now(),
            tokio::time::Instant::now() + Duration::from_secs(30),
            None,
            shutdown.clone(),
        );
        tokio::pin!(collection);
        assert!(futures::poll!(&mut collection).is_pending());
        shutdown.cancel();
        let outcomes = timeout(Duration::from_secs(1), collection).await.unwrap();
        assert_eq!(outcomes[0].kind, TurnDeliveryOutcomeKind::ShutdownCancelled);
        assert!(
            written_tx.send(Instant::now()).is_err(),
            "shutdown must release the writer receiver"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn turn_delivery_enqueue_classification_preserves_timeout_shutdown_and_closed() {
        let notification = || OutgoingEnvelope::Broadcast {
            message: OutgoingMessage::AppServerNotification(turn_completed_notification(
                ThreadId::new(),
                "enqueue-turn",
            )),
        };
        let (sender, receiver) = mpsc::channel(1);
        sender.try_send(notification()).unwrap();
        let timed_out =
            Some(tokio::time::timeout(Duration::from_millis(1), sender.send(notification())).await);
        assert_eq!(
            turn_enqueue_outcome(&timed_out),
            Some(TurnDeliveryOutcomeKind::Timeout)
        );
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let shutdown = tokio::select! {
            biased;
            _ = cancelled.cancelled() => None,
            result = tokio::time::timeout(Duration::from_secs(1), sender.send(notification())) => Some(result),
        };
        assert_eq!(
            turn_enqueue_outcome(&shutdown),
            Some(TurnDeliveryOutcomeKind::ShutdownCancelled)
        );
        drop(receiver);
        let closed =
            Some(tokio::time::timeout(Duration::from_secs(1), sender.send(notification())).await);
        assert_eq!(
            turn_enqueue_outcome(&closed),
            Some(TurnDeliveryOutcomeKind::Failure)
        );
        for kind in [
            TurnDeliveryOutcomeKind::Timeout,
            TurnDeliveryOutcomeKind::ShutdownCancelled,
            TurnDeliveryOutcomeKind::Failure,
        ] {
            let outcomes = collect_turn_delivery_outcomes(
                vec![PendingTurnDeliveryReceipt {
                    connection_id: ConnectionId(1),
                    receiver: None,
                    filtered: None,
                    immediate_outcome: Some(kind),
                }],
                Instant::now(),
                tokio::time::Instant::now(),
                None,
                CancellationToken::new(),
            )
            .await;
            assert_eq!(
                outcomes[0].kind, kind,
                "receipt collection must retain enqueue outcome"
            );
        }
    }

    #[tokio::test]
    async fn receipt_latency_uses_writer_time_even_when_later_enqueue_delays_collection() {
        let started = Instant::now() - Duration::from_secs(1);
        let (first_tx, first_rx) = oneshot::channel();
        first_tx.send(started + Duration::from_millis(2)).unwrap();
        // Collection cannot begin until the later recipient's enqueue settles.
        let (second_tx, second_rx) = oneshot::channel();
        second_tx.send(started + Duration::from_millis(80)).unwrap();
        // Both writer timestamps precede collection without a wall-clock sleep.
        let outcomes = collect_turn_delivery_outcomes(
            vec![
                PendingTurnDeliveryReceipt {
                    connection_id: ConnectionId(1),
                    receiver: Some(first_rx),
                    filtered: None,
                    immediate_outcome: None,
                },
                PendingTurnDeliveryReceipt {
                    connection_id: ConnectionId(2),
                    receiver: Some(second_rx),
                    filtered: None,
                    immediate_outcome: None,
                },
            ],
            started,
            tokio::time::Instant::now() + Duration::from_secs(1),
            Some(started - Duration::from_millis(5)),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(outcomes[0].successful_elapsed_ms, Some(2));
        assert_eq!(outcomes[0].post_core_delivery_latency_ms, Some(7));
        assert_eq!(outcomes[1].successful_elapsed_ms, Some(80));
        assert_eq!(outcomes[1].post_core_delivery_latency_ms, Some(85));
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
                filtered: None,
                immediate_outcome: None,
            }],
            Instant::now(),
            tokio::time::Instant::now() + Duration::from_secs(30),
            Some(Instant::now()),
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
                open_connection_request.clone(),
                tracing::info_span!("app_server.request", rpc.method = "turn/start"),
                /*parent_trace*/ None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 2);

        outgoing.connection_closed(ConnectionId(9)).await;

        let contexts = outgoing.request_contexts.lock().await;
        assert_eq!(contexts.len(), 1);
        assert!(contexts.contains_key(&open_connection_request));
    }

    #[tokio::test]
    async fn notify_client_error_forwards_error_to_waiter() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        outgoing
            .connection_opened(ConnectionId(1), Arc::new(AtomicBool::new(true)))
            .await;

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
            .notify_client_error(ConnectionId(1), request_id, error.clone())
            .await;

        let result = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("wait should not time out")
            .expect("waiter should receive a callback");
        assert_eq!(result, Err(error));
    }

    #[tokio::test]
    async fn request_id_exhaustion_preserves_pending_app_server_correlation() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(1);
        outgoing
            .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
            .await;
        let thread_id = ThreadId::new();
        let scoped = ThreadScopedOutgoingMessageSender::new(
            Arc::clone(&outgoing),
            vec![connection_id],
            thread_id,
        );
        let request = || {
            ServerRequestPayload::FileChangeRequestApproval(FileChangeRequestApprovalParams {
                thread_id: thread_id.to_string(),
                turn_id: "retained-turn".to_string(),
                item_id: "retained-item".to_string(),
                started_at_ms: 0,
                reason: None,
                grant_root: None,
            })
        };
        outgoing
            .next_server_request_id
            .store(i64::MAX - 1, Ordering::Relaxed);
        let (pending_id, pending_response) = scoped.send_request(request()).await;
        let pending_id = pending_id.expect("last request before exhaustion must be admitted");
        let OutgoingEnvelope::ToConnection {
            connection_id: recipient,
            message: OutgoingMessage::Request(emitted),
            ..
        } = rx.recv().await.expect("request emitted")
        else {
            panic!("expected targeted approval request");
        };
        assert_eq!(recipient, connection_id);
        assert_eq!(pending_id, RequestId::Integer(i64::MAX - 1));
        assert_eq!(emitted.id(), &pending_id);

        let expected_error = internal_error("app-server request IDs are exhausted");
        for _ in 0..2 {
            let (rejected_id, rejected_response) = scoped.send_request(request()).await;
            assert_eq!(
                rejected_id, None,
                "rejection must not fabricate a callback identity"
            );
            assert_eq!(
                rejected_response.await.expect("rejection must resolve"),
                Err(expected_error.clone())
            );
            assert_eq!(outgoing.pending_callback_count().await, 1);
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        let direct_error = outgoing
            .send_request_to_connections(Some(&[connection_id]), request(), Some(thread_id))
            .await
            .expect_err("direct caller must observe exhaustion");
        assert_eq!(direct_error, expected_error);
        assert_eq!(
            outgoing
                .send_request_to_connections_and_wait(
                    Some(&[connection_id]),
                    request(),
                    Some(thread_id)
                )
                .await
                .expect("wait wrapper must return its internal error"),
            Err(expected_error)
        );
        assert_eq!(outgoing.pending_callback_count().await, 1);

        let expected = json!({"decision": "accept"});
        outgoing
            .notify_client_response(connection_id, pending_id, expected.clone())
            .await;
        assert_eq!(
            pending_response
                .await
                .expect("retained response must arrive"),
            Ok(expected)
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
        assert_eq!(
            outgoing.next_server_request_id.load(Ordering::Relaxed),
            i64::MAX
        );
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn broadcast_request_without_initialized_connections_is_canceled_immediately() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());

        let (_request_id, wait_for_result) = outgoing
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

        let error = wait_for_result
            .await
            .expect("request should be canceled explicitly")
            .expect_err("request without a recipient should fail");
        assert_eq!(
            error.message,
            "client request canceled because no initialized connections are available"
        );
        assert!(
            rx.try_recv().is_err(),
            "request without recipients must not be enqueued",
        );
    }

    #[tokio::test]
    async fn legacy_exec_approval_emission_is_rejected() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        outgoing
            .connection_opened(ConnectionId(1), Arc::new(AtomicBool::new(true)))
            .await;

        let (_request_id, wait_for_result) = outgoing
            .send_request(ServerRequestPayload::ExecCommandApproval(
                ExecCommandApprovalParams {
                    conversation_id: ThreadId::new(),
                    call_id: "call-id".to_string(),
                    approval_id: None,
                    command: vec!["echo".to_string(), "hello".to_string()],
                    cwd: std::path::PathBuf::from("."),
                    reason: None,
                    parsed_cmd: Vec::new(),
                },
            ))
            .await;

        let error = wait_for_result
            .await
            .expect("legacy request should resolve explicitly")
            .expect_err("legacy request emission must be rejected");
        assert_eq!(error.message, LEGACY_EXEC_COMMAND_APPROVAL_EMISSION_ERROR);
        assert!(rx.try_recv().is_err(), "no legacy request may be emitted");
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn broadcast_request_freezes_delivery_and_authorization_to_initialized_connections() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let original_connection = ConnectionId(21);
        let later_initialized_connection = ConnectionId(22);
        let later_initialized = Arc::new(AtomicBool::new(/*value*/ false));
        outgoing
            .connection_opened(
                original_connection,
                Arc::new(AtomicBool::new(/*value*/ true)),
            )
            .await;
        outgoing
            .connection_opened(later_initialized_connection, Arc::clone(&later_initialized))
            .await;

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
        later_initialized.store(true, Ordering::Release);

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("initialized connection should receive the request before timeout")
            .expect("outgoing channel should stay open");
        let OutgoingEnvelope::ToConnection { connection_id, .. } = envelope else {
            panic!("broadcast request should use its frozen targeted delivery set");
        };
        assert_eq!(connection_id, original_connection);
        assert!(
            rx.try_recv().is_err(),
            "a connection initialized after the snapshot must not receive the request",
        );
        outgoing
            .notify_client_error(
                later_initialized_connection,
                request_id.clone(),
                internal_error("unauthorized"),
            )
            .await;

        let expected_error = internal_error("authorized");
        outgoing
            .notify_client_error(original_connection, request_id, expected_error.clone())
            .await;
        let result = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("authorized response should resolve the callback")
            .expect("callback sender should remain available");
        assert_eq!(result, Err(expected_error));
    }

    #[tokio::test]
    async fn connection_closed_cancels_non_thread_request_after_last_recipient_disconnects() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let connection_id = ConnectionId(31);
        outgoing
            .connection_opened(connection_id, Arc::new(AtomicBool::new(/*value*/ true)))
            .await;
        let (_request_id, wait_for_result) = outgoing
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

        outgoing.connection_closed(connection_id).await;

        let error = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("disconnect should resolve the callback")
            .expect("callback sender should report a cancellation")
            .expect_err("disconnect should cancel the request");
        assert_eq!(
            error.message,
            "client request canceled because all authorized connections disconnected"
        );
    }

    // The test thread holds the callback map while a separate runtime worker
    // enters registration. A second worker remains available for disconnect.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_request_registration_is_atomic_with_disconnect_cleanup() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(32);
        outgoing
            .connection_opened(connection_id, Arc::new(AtomicBool::new(/*value*/ true)))
            .await;
        let thread_id = ThreadId::new();
        let thread_outgoing =
            ThreadScopedOutgoingMessageSender::new_with_experimental_api_connections(
                Arc::clone(&outgoing),
                vec![connection_id],
                vec![connection_id],
                thread_id,
            );

        let callback_guard = outgoing
            .request_id_to_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let send_task = tokio::spawn(async move {
            thread_outgoing
                .send_request(ServerRequestPayload::DynamicToolCall(
                    DynamicToolCallParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn-1".to_string(),
                        call_id: "call-id".to_string(),
                        namespace: None,
                        tool: "test_tool".to_string(),
                        arguments: json!({}),
                    },
                ))
                .await
        });
        timeout(Duration::from_secs(1), async {
            while outgoing.active_connections.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("registration enters active-to-callback critical section");
        assert!(
            outgoing.active_connections.try_lock().is_err(),
            "request registration must retain the active-connection snapshot lock",
        );

        let close_task = outgoing.connection_closed(connection_id);
        tokio::pin!(close_task);
        assert!(
            futures::poll!(&mut close_task).is_pending(),
            "disconnect cleanup must wait until callback registration completes",
        );

        drop(callback_guard);
        let (_request_id, wait_for_result) = timeout(Duration::from_secs(1), send_task)
            .await
            .expect("request registration should finish")
            .expect("request registration task should not panic");
        timeout(Duration::from_secs(1), close_task)
            .await
            .expect("disconnect cleanup should finish");
        let error = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("disconnect should resolve the callback")
            .expect("callback sender should report a cancellation")
            .expect_err("disconnect should cancel the request");
        assert_eq!(error.message, ALL_AUTHORIZED_CONNECTIONS_DISCONNECTED_ERROR);
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    fn fanout_test_request(thread_id: ThreadId) -> ServerRequestPayload {
        ServerRequestPayload::FileChangeRequestApproval(FileChangeRequestApprovalParams {
            thread_id: thread_id.to_string(),
            turn_id: "turn".into(),
            item_id: "item".into(),
            started_at_ms: 0,
            reason: None,
            grant_root: None,
        })
    }

    #[tokio::test]
    async fn settled_request_wakes_backpressured_fanout() {
        for cancel in [false, true] {
            let (tx, mut rx) = mpsc::channel(1);
            let outgoing = Arc::new(OutgoingMessageSender::new(
                tx,
                AnalyticsEventsClient::disabled(),
            ));
            let connections = [ConnectionId(1), ConnectionId(2), ConnectionId(3)];
            for id in connections {
                outgoing
                    .connection_opened(id, Arc::new(AtomicBool::new(true)))
                    .await;
            }
            let thread_id = ThreadId::new();
            let send = tokio::spawn({
                let outgoing = Arc::clone(&outgoing);
                async move {
                    outgoing
                        .send_request_to_connections(
                            Some(&connections),
                            fanout_test_request(thread_id),
                            Some(thread_id),
                        )
                        .await
                }
            });
            let first = rx.recv().await.unwrap();
            let OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::Request(request),
                ..
            } = first
            else {
                panic!("request");
            };
            let id = request.id().clone();
            timeout(Duration::from_secs(1), async {
                while rx.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!send.is_finished());
            if cancel {
                outgoing
                    .cancel_requests_for_thread(thread_id, Some(internal_error("cancelled")))
                    .await;
            } else {
                outgoing
                    .notify_client_response(connections[0], id, json!({"decision":"accept"}))
                    .await;
            }
            let (_, result) = timeout(Duration::from_secs(1), send)
                .await
                .expect("settlement wakes capacity wait")
                .unwrap()
                .unwrap();
            assert_eq!(
                result.await.unwrap(),
                if cancel {
                    Err(internal_error("cancelled"))
                } else {
                    Ok(json!({"decision": "accept"}))
                },
            );
            assert_eq!(
                rx.len(),
                1,
                "second request remains queued; third is never published"
            );
        }
    }

    #[tokio::test]
    async fn ordinary_notification_timeout_invalidates_the_failed_route() {
        let (tx, _rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            AnalyticsEventsClient::disabled(),
        ));
        let connection = ConnectionId(1);
        let failure = CancellationToken::new();
        outgoing
            .connection_opened_with_runtime(
                connection,
                Arc::new(AtomicBool::new(true)),
                failure.clone(),
            )
            .await;
        let notification = turn_completed_notification(ThreadId::new(), "turn");
        outgoing
            .send_server_notification_to_connections(&[connection], notification.clone())
            .await;
        tokio::time::pause();
        let send = tokio::spawn({
            let outgoing = Arc::clone(&outgoing);
            async move {
                outgoing
                    .send_server_notification_to_connections(&[connection], notification)
                    .await
            }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(RESOURCE_DELIVERY_TIMEOUT).await;
        send.await.unwrap();
        assert!(
            failure.is_cancelled(),
            "failed lifecycle delivery must invalidate its route"
        );
    }

    #[tokio::test]
    async fn dynamic_tool_dispatch_has_one_registered_executor() {
        let (tx, mut rx) = mpsc::channel(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            AnalyticsEventsClient::disabled(),
        ));
        let connections = [ConnectionId(1), ConnectionId(2)];
        for id in connections {
            outgoing
                .connection_opened(id, Arc::new(AtomicBool::new(true)))
                .await;
        }
        let thread_id = ThreadId::new();
        outgoing
            .register_dynamic_tool_owner(thread_id, connections[1])
            .await;
        let (request_id, result) = outgoing
            .send_request_to_connections(
                Some(&connections),
                ServerRequestPayload::DynamicToolCall(DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn".into(),
                    call_id: "call".into(),
                    namespace: None,
                    tool: "tool".into(),
                    arguments: json!({}),
                }),
                Some(thread_id),
            )
            .await
            .unwrap();
        let mut executions = 0;
        while let Ok(envelope) = rx.try_recv() {
            let OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::Request(_),
                ..
            } = envelope
            else {
                panic!("request");
            };
            assert_eq!(connection_id, connections[1]);
            executions += 1;
        }
        assert_eq!(executions, 1);
        outgoing
            .replay_requests_to_connection_for_thread(connections[0], thread_id, true)
            .await;
        outgoing
            .replay_requests_to_connection_for_thread(connections[1], thread_id, true)
            .await;
        assert!(rx.try_recv().is_err());
        let expected = json!({
            "success": true,
            "contentItems": [{"type": "inputText", "text": "x".repeat(1024 * 1024)}],
        });
        outgoing
            .notify_client_response(connections[1], request_id, expected.clone())
            .await;
        assert_eq!(result.await.unwrap(), Ok(expected));
    }

    #[tokio::test]
    async fn dynamic_tool_replay_never_transfers_execution_authority() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let original_connection = ConnectionId(41);
        let resumed_connection = ConnectionId(42);
        outgoing
            .connection_opened(
                original_connection,
                Arc::new(AtomicBool::new(/*value*/ true)),
            )
            .await;
        outgoing
            .connection_opened(
                resumed_connection,
                Arc::new(AtomicBool::new(/*value*/ true)),
            )
            .await;
        let thread_outgoing =
            ThreadScopedOutgoingMessageSender::new_with_experimental_api_connections(
                Arc::clone(&outgoing),
                vec![original_connection],
                vec![original_connection],
                thread_id,
            );
        let (request_id, wait_for_result) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-1".to_string(),
                    namespace: None,
                    tool: "test_tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let request_id = request_id.expect("request admitted");
        let _original_delivery = rx.recv().await.expect("original request delivery");

        outgoing
            .replay_requests_to_connection_for_thread(
                resumed_connection,
                thread_id,
                /*experimental_api_enabled*/ false,
            )
            .await;
        assert!(
            rx.try_recv().is_err(),
            "stable replay must emit no dynamic tool request"
        );
        assert_eq!(outgoing.pending_callback_count().await, 1);
        outgoing
            .replay_requests_to_connection_for_thread(
                resumed_connection,
                thread_id,
                /*experimental_api_enabled*/ true,
            )
            .await;

        assert!(
            rx.try_recv().is_err(),
            "resuming must not reexecute pending work"
        );
        let expected_result = json!({"contentItems": [], "success": true});
        outgoing
            .notify_client_response(
                resumed_connection,
                request_id.clone(),
                expected_result.clone(),
            )
            .await;
        assert_eq!(
            outgoing.pending_callback_count().await,
            1,
            "observer cannot answer"
        );
        outgoing
            .notify_client_response(original_connection, request_id, expected_result.clone())
            .await;
        let result = wait_for_result
            .await
            .expect("original callback sender should remain available");
        assert_eq!(result, Ok(expected_result));
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn replay_does_not_enqueue_request_resolved_on_another_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let original_connection = ConnectionId(43);
        let resumed_connection = ConnectionId(44);
        for connection_id in [original_connection, resumed_connection] {
            outgoing
                .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                .await;
        }
        let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
            Arc::clone(&outgoing),
            vec![original_connection],
            thread_id,
        );
        let (request_id, wait_for_result) = thread_outgoing
            .send_request(ServerRequestPayload::AttestationGenerate(
                AttestationGenerateParams {},
            ))
            .await;
        let request_id = request_id.expect("request admitted");

        let replay_task = outgoing.replay_requests_to_connection_for_thread(
            resumed_connection,
            thread_id,
            /*experimental_api_enabled*/ false,
        );
        tokio::pin!(replay_task);
        assert!(
            futures::poll!(&mut replay_task).is_pending(),
            "the full channel should hold replay at capacity reservation"
        );

        let expected_result = json!({"token": "resolved"});
        outgoing
            .notify_client_response(original_connection, request_id, expected_result.clone())
            .await;
        assert_eq!(
            wait_for_result
                .await
                .expect("original connection should resolve the callback"),
            Ok(expected_result)
        );
        timeout(Duration::from_secs(1), replay_task)
            .await
            .expect("settlement must retire replay while output stays full");
        let _original_delivery = rx.recv().await.expect("original request delivery");

        assert!(
            rx.try_recv().is_err(),
            "a request resolved while replay waited must not be enqueued"
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn dynamic_tool_request_without_active_experimental_recipient_is_terminal() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let stable_connection = ConnectionId(50);
        outgoing
            .connection_opened(stable_connection, Arc::new(AtomicBool::new(/*value*/ true)))
            .await;
        let thread_outgoing =
            ThreadScopedOutgoingMessageSender::new_with_experimental_api_connections(
                Arc::clone(&outgoing),
                vec![stable_connection],
                Vec::new(),
                thread_id,
            );

        let (_request_id, wait_for_result) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-0".to_string(),
                    namespace: None,
                    tool: "test_tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;

        let error = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("zero-recipient request should resolve promptly")
            .expect("callback sender should return an error")
            .expect_err("zero-recipient request must be rejected");
        assert_eq!(error.message, NO_ACTIVE_AUTHORIZED_CONNECTIONS_ERROR);
        assert!(rx.try_recv().is_err(), "no request envelope may be emitted");
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn partial_multi_connection_send_keeps_the_delivered_request_callback() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let first_connection = ConnectionId(60);
        let second_connection = ConnectionId(61);
        for connection_id in [first_connection, second_connection] {
            outgoing
                .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                .await;
        }
        let request = fanout_test_request(ThreadId::new());

        let send_outgoing = Arc::clone(&outgoing);
        let send_task = tokio::spawn(async move {
            send_outgoing
                .send_request_to_connections(
                    Some(&[first_connection, second_connection]),
                    request,
                    /*thread_id*/ None,
                )
                .await
                .expect("request admitted")
        });
        timeout(Duration::from_secs(1), async {
            while rx.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first recipient fills transport before partial failure");
        rx.close();
        let first_delivery = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first delivery should not time out")
            .expect("first delivery should be queued");
        let delivered_connection = match first_delivery {
            OutgoingEnvelope::ToConnection { connection_id, .. } => connection_id,
            envelope => panic!("unexpected first delivery: {envelope:?}"),
        };
        assert_eq!(delivered_connection, first_connection);

        let (request_id, wait_for_result) = timeout(Duration::from_secs(1), send_task)
            .await
            .expect("send should finish after the receiver closes")
            .expect("send task should not panic");
        assert_eq!(outgoing.pending_callback_count().await, 1);

        assert!(
            rx.recv().await.is_none(),
            "second recipient was never published"
        );
        outgoing
            .notify_client_response(
                second_connection,
                request_id.clone(),
                json!({"unauthorized": true}),
            )
            .await;
        assert_eq!(outgoing.pending_callback_count().await, 1);
        let expected_result = json!({"contentItems": [], "success": true});
        outgoing
            .notify_client_response(delivered_connection, request_id, expected_result.clone())
            .await;
        assert_eq!(
            timeout(Duration::from_secs(1), wait_for_result)
                .await
                .expect("delivered request should resolve promptly")
                .expect("callback sender should remain live"),
            Ok(expected_result)
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn initial_request_delivery_never_publishes_a_canceled_callback_after_backpressure() {
        verify_initial_request_delivery_failure(/*cancel_before_capacity*/ true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn initial_request_delivery_budget_releases_callback_without_draining_transport() {
        verify_initial_request_delivery_failure(/*cancel_before_capacity*/ false).await;
    }

    async fn verify_initial_request_delivery_failure(cancel_before_capacity: bool) {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(64);
        outgoing
            .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
            .await;
        let thread_id = ThreadId::new();
        outgoing
            .send_server_notification(ServerNotification::ThreadClosed(
                codex_app_server_protocol::ThreadClosedNotification {
                    thread_id: "transport-capacity-sentinel".to_string(),
                },
            ))
            .await;
        let request = fanout_test_request(ThreadId::new());
        let send_outgoing = Arc::clone(&outgoing);
        let send_task = tokio::spawn(async move {
            send_outgoing
                .send_request_to_connections(Some(&[connection_id]), request, Some(thread_id))
                .await
                .expect("request admitted")
        });
        timeout(Duration::from_secs(1), async {
            while outgoing.pending_callback_count().await != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("normal request registration must precede capacity admission");
        assert!(
            !send_task.is_finished(),
            "actual output queue must block delivery"
        );
        let expected_error = if cancel_before_capacity {
            let error = internal_error("thread request was canceled before delivery");
            outgoing
                .cancel_requests_for_thread(thread_id, Some(error.clone()))
                .await;
            assert_eq!(outgoing.pending_callback_count().await, 0);
            rx.recv().await.expect("remove the capacity sentinel");
            error
        } else {
            internal_error(ALL_AUTHORIZED_CONNECTION_SENDS_FAILED_ERROR)
        };
        let (_, result) = timeout(Duration::from_secs(7), send_task)
            .await
            .expect("delivery must terminate even while transport remains full")
            .expect("request delivery task must not panic");
        assert_eq!(
            result.await.expect("callback reports the failure"),
            Err(expected_error)
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
        if !cancel_before_capacity {
            assert_eq!(
                rx.len(),
                1,
                "capacity sentinel remains queued throughout the budget"
            );
            rx.recv()
                .await
                .expect("remove the original capacity sentinel");
        }
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "neither cancellation nor delivery timeout may emit a stale request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn partial_request_delivery_timeout_releases_callback_after_delivered_peer_disconnects() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let first = ConnectionId(65);
        let second = ConnectionId(66);
        for connection_id in [first, second] {
            outgoing
                .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                .await;
        }
        let thread_id = ThreadId::new();
        let request = fanout_test_request(ThreadId::new());
        let send_outgoing = Arc::clone(&outgoing);
        let send_task = tokio::spawn(async move {
            send_outgoing
                .send_request_to_connections(Some(&[first, second]), request, Some(thread_id))
                .await
                .expect("request admitted")
        });
        timeout(Duration::from_secs(1), async {
            while rx.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first recipient delivery must fill the actual output queue");
        outgoing.connection_closed(first).await;
        assert_eq!(outgoing.pending_callback_count().await, 1);
        let (_, result) = timeout(Duration::from_secs(7), send_task)
            .await
            .expect("second recipient delivery must exhaust its budget")
            .expect("delivery owner must not panic");
        assert_eq!(
            result
                .await
                .expect("removed final authority must resolve callback"),
            Err(internal_error(ALL_AUTHORIZED_CONNECTION_SENDS_FAILED_ERROR))
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
        assert!(matches!(
            rx.recv().await,
            Some(OutgoingEnvelope::ToConnection { connection_id, .. }) if connection_id == first
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn initial_delivery_timeout_preserves_a_concurrently_replayed_recipient() {
        verify_initial_delivery_replay_authority(false).await;
        verify_initial_delivery_replay_authority(true).await;
    }

    async fn verify_initial_delivery_replay_authority(replay_original_target: bool) {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let first = ConnectionId(67);
        let second = ConnectionId(68);
        let last = ConnectionId(70);
        let replayed = if replay_original_target {
            last
        } else {
            ConnectionId(69)
        };
        for connection_id in [first, second, replayed, last] {
            outgoing
                .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                .await;
        }
        let thread_id = ThreadId::new();
        let request = fanout_test_request(ThreadId::new());
        let targets = [first, second, last];
        let mut initial = Box::pin(outgoing.send_request_to_connections(
            Some(&targets),
            request,
            Some(thread_id),
        ));
        assert!(futures::poll!(initial.as_mut()).is_pending());
        let mut replay =
            Box::pin(outgoing.replay_requests_to_connection_for_thread(replayed, thread_id, true));
        assert!(futures::poll!(replay.as_mut()).is_pending());
        assert!(matches!(
            rx.recv().await,
            Some(OutgoingEnvelope::ToConnection { connection_id, .. }) if connection_id == first
        ));
        assert!(futures::poll!(initial.as_mut()).is_pending());
        assert!(matches!(
            rx.recv().await,
            Some(OutgoingEnvelope::ToConnection { connection_id, .. }) if connection_id == second
        ));
        timeout(Duration::from_secs(1), replay)
            .await
            .expect("normal replay must publish before the final initial target");
        outgoing.connection_closed(first).await;
        outgoing.connection_closed(second).await;
        let (request_id, result) = timeout(Duration::from_secs(7), initial)
            .await
            .expect("last initial target remains blocked until delivery timeout")
            .expect("request admitted");
        assert_eq!(outgoing.pending_callback_count().await, 1);
        assert!(matches!(
            rx.recv().await,
            Some(OutgoingEnvelope::ToConnection { connection_id, .. }) if connection_id == replayed
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let expected = json!({"contentItems": [], "success": true});
        outgoing
            .notify_client_response(replayed, request_id, expected.clone())
            .await;
        assert_eq!(
            result
                .await
                .expect("replayed recipient retains callback authority"),
            Ok(expected)
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn replay_capacity_wait_is_bounded_and_does_not_authorize_undelivered_requests() {
        tokio::time::pause();
        for shutdown in [false, true] {
            let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
            let outgoing =
                OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
            let original = ConnectionId(71);
            let replayed = ConnectionId(72);
            for connection_id in [original, replayed] {
                outgoing
                    .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                    .await;
            }
            let thread_id = ThreadId::new();
            let (request_id, result) = outgoing
                .send_request_to_connections(
                    Some(&[original]),
                    fanout_test_request(ThreadId::new()),
                    Some(thread_id),
                )
                .await
                .expect("request admitted");
            let mut replay = Box::pin(
                outgoing.replay_requests_to_connection_for_thread(replayed, thread_id, true),
            );
            assert!(futures::poll!(replay.as_mut()).is_pending());
            if shutdown {
                outgoing.delivery_shutdown.cancel();
                timeout(Duration::from_millis(1), replay)
                    .await
                    .expect("shutdown releases replay immediately");
            } else {
                timeout(RESOURCE_DELIVERY_TIMEOUT + Duration::from_millis(1), replay)
                    .await
                    .expect("replay has its own delivery deadline");
            }
            assert!(matches!(rx.recv().await,
                Some(OutgoingEnvelope::ToConnection { connection_id, .. }) if connection_id == original));
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            outgoing
                .notify_client_response(replayed, request_id.clone(), json!({"stale": true}))
                .await;
            assert_eq!(outgoing.pending_callback_count().await, 1);
            let expected = json!({"contentItems": [], "success": true});
            outgoing
                .notify_client_response(original, request_id, expected.clone())
                .await;
            assert_eq!(
                result.await.expect("original callback remains live"),
                Ok(expected)
            );
        }
    }

    #[tokio::test]
    async fn thread_cleanup_evicts_token_usage_and_resume_delivers_unchanged_usage() {
        let (tx, mut rx) = mpsc::channel(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            AnalyticsEventsClient::disabled(),
        ));
        let closed_thread = ThreadId::new();
        let other_thread = ThreadId::new();
        let notification = |thread_id: ThreadId| {
            let usage = json!({"totalTokens": 1, "inputTokens": 1, "cachedInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0});
            ServerNotification::ThreadTokenUsageUpdated(
                serde_json::from_value(json!({
                    "threadId": thread_id.to_string(), "turnId": "turn",
                    "tokenUsage": {"total": usage, "last": usage, "modelContextWindow": null}
                }))
                .unwrap(),
            )
        };
        let closed = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1), ConnectionId(2)],
            closed_thread,
        );
        let other = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            other_thread,
        );
        closed
            .send_component_notification_if_changed(notification(closed_thread))
            .await;
        other
            .send_component_notification_if_changed(notification(other_thread))
            .await;
        for _ in 0..3 {
            rx.try_recv().expect("initial usage delivered");
        }
        outgoing
            .cancel_requests_for_thread(closed_thread, None)
            .await;
        assert_eq!(
            outgoing
                .component_notification_cache
                .lock()
                .await
                .token_usage
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![(ConnectionId(1), other_thread.to_string())]
        );
        other
            .send_component_notification_if_changed(notification(other_thread))
            .await;
        assert!(
            rx.try_recv().is_err(),
            "unrelated thread remains deduplicated"
        );
        closed
            .send_component_notification_if_changed(notification(closed_thread))
            .await;
        for expected_connection in [ConnectionId(1), ConnectionId(2)] {
            let OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::AppServerNotification(delivered),
                ..
            } = rx.try_recv().expect("resumed usage delivered")
            else {
                panic!("expected targeted usage");
            };
            assert_eq!(connection_id, expected_connection);
            assert_eq!(
                serde_json::to_value(delivered).unwrap(),
                serde_json::to_value(notification(closed_thread)).unwrap()
            );
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn replay_does_not_resurrect_recipient_disconnected_during_capacity_wait() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let original = ConnectionId(71);
        let replayed = ConnectionId(72);
        for connection_id in [original, replayed] {
            outgoing
                .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                .await;
        }
        let thread_id = ThreadId::new();
        let (request_id, result) = outgoing
            .send_request_to_connections(
                Some(&[original]),
                fanout_test_request(ThreadId::new()),
                Some(thread_id),
            )
            .await
            .expect("request admitted");
        let mut replay =
            Box::pin(outgoing.replay_requests_to_connection_for_thread(replayed, thread_id, true));
        assert!(futures::poll!(replay.as_mut()).is_pending());
        outgoing.connection_closed(replayed).await;
        timeout(Duration::from_secs(1), replay)
            .await
            .expect("disconnect must retire replay while output stays full");
        assert!(matches!(
            rx.recv().await,
            Some(OutgoingEnvelope::ToConnection { connection_id, .. }) if connection_id == original
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        outgoing
            .notify_client_response(replayed, request_id.clone(), json!({"stale": true}))
            .await;
        assert_eq!(outgoing.pending_callback_count().await, 1);
        let expected = json!({"contentItems": [], "success": true});
        outgoing
            .notify_client_response(original, request_id, expected.clone())
            .await;
        assert_eq!(
            result.await.expect("original recipient remains live"),
            Ok(expected)
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn canceled_multi_connection_send_removes_registered_callback() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let first_connection = ConnectionId(62);
        let second_connection = ConnectionId(63);
        for connection_id in [first_connection, second_connection] {
            outgoing
                .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                .await;
        }
        let request = fanout_test_request(ThreadId::new());

        let send_outgoing = Arc::clone(&outgoing);
        let send_task = tokio::spawn(async move {
            send_outgoing
                .send_request_to_connections(
                    Some(&[first_connection, second_connection]),
                    request,
                    /*thread_id*/ None,
                )
                .await
                .expect("request admitted")
        });
        timeout(Duration::from_secs(1), async {
            while outgoing.pending_callback_count().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request callback should be registered before cancellation");

        send_task.abort();
        let _canceled = send_task.await;
        timeout(Duration::from_secs(1), async {
            while outgoing.pending_callback_count().await != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceled send should remove its request callback");
    }

    #[test]
    fn canceled_send_after_runtime_shutdown_removes_callback_and_prevents_replay() {
        use std::future::Future;
        use std::task::Poll;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create originating runtime");
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connections = [ConnectionId(61), ConnectionId(62)];
        let thread_id = ThreadId::new();
        runtime.block_on(async {
            for connection_id in connections {
                outgoing
                    .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                    .await;
            }
        });
        let mut send = Box::pin(outgoing.send_request_to_connections(
            Some(&connections),
            ServerRequestPayload::AttestationGenerate(AttestationGenerateParams {}),
            Some(thread_id),
        ));
        runtime.block_on(std::future::poll_fn(|cx| {
            assert!(
                send.as_mut().poll(cx).is_pending(),
                "the second recipient must wait behind the full delivery queue"
            );
            Poll::Ready(())
        }));
        assert_eq!(runtime.block_on(outgoing.pending_callback_count()), 1);
        let delivered = rx
            .try_recv()
            .expect("first recipient should receive request");
        assert!(matches!(
            delivered,
            OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::Request(_),
                ..
            }
        ));

        drop(runtime);
        drop(send);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create reconnect runtime");
        runtime.block_on(async {
            assert_eq!(outgoing.pending_callback_count().await, 0);
            outgoing
                .replay_requests_to_connection_for_thread(connections[0], thread_id, true)
                .await;
        });
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "a canceled request must not be replayed after reconnect"
        );
    }

    #[tokio::test]
    async fn dynamic_tool_request_only_authorizes_experimental_connections() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let experimental_connection = ConnectionId(51);
        let stable_connection = ConnectionId(52);
        outgoing
            .connection_opened(
                experimental_connection,
                Arc::new(AtomicBool::new(/*value*/ true)),
            )
            .await;
        outgoing
            .connection_opened(stable_connection, Arc::new(AtomicBool::new(/*value*/ true)))
            .await;
        let thread_outgoing =
            ThreadScopedOutgoingMessageSender::new_with_experimental_api_connections(
                Arc::clone(&outgoing),
                vec![experimental_connection, stable_connection],
                vec![experimental_connection],
                thread_id,
            );

        let (request_id, wait_for_result) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-1".to_string(),
                    namespace: None,
                    tool: "test_tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let request_id = request_id.expect("request admitted");

        let delivery = rx.recv().await.expect("dynamic tool request delivery");
        let OutgoingEnvelope::ToConnection { connection_id, .. } = delivery else {
            panic!("dynamic tool request should use targeted delivery");
        };
        assert_eq!(connection_id, experimental_connection);
        assert!(
            rx.try_recv().is_err(),
            "stable connection must not receive the dynamic tool request"
        );

        outgoing
            .notify_client_response(
                stable_connection,
                request_id.clone(),
                json!({"contentItems": [], "success": false}),
            )
            .await;
        outgoing
            .notify_client_error(
                stable_connection,
                request_id.clone(),
                internal_error("method not found"),
            )
            .await;
        assert_eq!(
            outgoing.pending_requests_for_thread(thread_id).await.len(),
            1
        );
        assert_eq!(outgoing.pending_callback_count().await, 1);

        let expected_result = json!({"contentItems": [], "success": true});
        outgoing
            .notify_client_response(experimental_connection, request_id, expected_result.clone())
            .await;
        assert_eq!(
            wait_for_result
                .await
                .expect("callback should remain available"),
            Ok(expected_result)
        );
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }

    #[tokio::test]
    async fn cancel_requests_for_thread_cancels_all_thread_requests() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        outgoing
            .connection_opened(ConnectionId(1), Arc::new(AtomicBool::new(true)))
            .await;
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

    #[tokio::test]
    async fn targeted_request_rejects_response_from_other_connection() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let allowed_connection = ConnectionId(11);
        let other_connection = ConnectionId(12);
        outgoing
            .connection_opened(
                allowed_connection,
                Arc::new(AtomicBool::new(/*value*/ true)),
            )
            .await;
        let (request_id, wait_for_result) = outgoing
            .send_request_to_connections(
                Some(&[allowed_connection]),
                ServerRequestPayload::ToolRequestUserInput(ToolRequestUserInputParams {
                    thread_id: ThreadId::new().to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "item-1".to_string(),
                    questions: vec![],
                    auto_resolution_ms: None,
                }),
                /*thread_id*/ None,
            )
            .await
            .expect("request admitted");

        outgoing
            .notify_client_response(
                other_connection,
                request_id.clone(),
                json!({"unauthorized": true}),
            )
            .await;
        assert_eq!(outgoing.pending_callback_count().await, 1);

        outgoing
            .notify_client_response(allowed_connection, request_id, json!({"answers": {}}))
            .await;
        let result = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("authorized response should resolve the callback")
            .expect("callback sender should remain available");
        assert_eq!(result, Ok(json!({"answers": {}})));
    }
    #[tokio::test]
    async fn outstanding_request_lease_survives_handler_return_until_response_or_disconnect() {
        let (sender, mut receiver) = mpsc::channel(128);
        let outgoing = OutgoingMessageSender::new(sender, AnalyticsEventsClient::disabled());
        let connection = ConnectionId(701);
        outgoing
            .connection_opened(connection, Arc::new(AtomicBool::new(true)))
            .await;
        for id in 0..64 {
            let mut context = RequestContext::new(
                ConnectionRequestId {
                    connection_id: connection,
                    request_id: RequestId::Integer(id),
                },
                tracing::Span::none(),
                None,
            );
            assert!(outgoing.admit_request_work(&mut context, 1024, false).await);
            // The handler drops its copy while an asynchronous completion remains pending.
        }
        let mut extra = RequestContext::new(
            ConnectionRequestId {
                connection_id: connection,
                request_id: RequestId::Integer(64),
            },
            tracing::Span::none(),
            None,
        );
        assert!(!outgoing.admit_request_work(&mut extra, 1024, false).await);
        let mut control = RequestContext::new(
            ConnectionRequestId {
                connection_id: connection,
                request_id: RequestId::Integer(65),
            },
            tracing::Span::none(),
            None,
        );
        assert!(outgoing.admit_request_work(&mut control, 0, true).await);
        drop(control);
        outgoing
            .send_error(
                ConnectionRequestId {
                    connection_id: connection,
                    request_id: RequestId::Integer(0),
                },
                internal_error("finished"),
            )
            .await;
        assert!(receiver.recv().await.is_some());
        assert!(outgoing.admit_request_work(&mut extra, 1024, false).await);
        drop(extra);
        outgoing.connection_closed(connection).await;
        assert_eq!(outgoing.outstanding.available_permits(), 1024);
        assert_eq!(outgoing.outstanding_control.available_permits(), 64);
        assert_eq!(
            outgoing.outstanding_bytes.available_permits(),
            32 * 1024 * 1024
        );
    }

    #[tokio::test]
    async fn control_envelopes_remain_bounded_and_independent_of_ordinary_bytes() {
        let (sender, _receiver) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(sender, AnalyticsEventsClient::disabled());
        let connection = ConnectionId(702);
        outgoing
            .connection_opened(connection, Arc::new(AtomicBool::new(true)))
            .await;
        let context = |id| {
            RequestContext::new(
                ConnectionRequestId {
                    connection_id: connection,
                    request_id: RequestId::Integer(id),
                },
                tracing::Span::none(),
                None,
            )
        };
        assert!(
            outgoing
                .admit_request_work(&mut context(0), 16 * 1024 * 1024, false)
                .await
        );
        assert!(!outgoing.admit_request_work(&mut context(1), 1, false).await);
        assert!(
            outgoing
                .admit_request_work(&mut context(2), 64 * 1024, true)
                .await
        );
        assert!(
            !outgoing
                .admit_request_work(&mut context(3), 64 * 1024 + 1, true)
                .await
        );
        outgoing.connection_closed(connection).await;
        assert_eq!(outgoing.outstanding_control.available_permits(), 64);
        assert_eq!(
            outgoing.outstanding_bytes.available_permits(),
            32 * 1024 * 1024
        );
    }
}
