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


pub(crate) type ClientRequestResult = std::result::Result<Result, JSONRPCErrorError>;

const TURN_DELIVERY_RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);
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

    /// Reserve notification capacity while a resource owner independently drains
    /// its pipes. Keep this future in that owner's select loop, never await it
    /// instead of draining; unadmitted bytes belong in the terminal suffix.
    pub(crate) async fn reserve_server_notification_capacity<'a>(
        &'a self,
        cancellation: &CancellationToken,
    ) -> Option<mpsc::Permit<'a, OutgoingEnvelope>> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            _ = self.delivery_shutdown.cancelled() => None,
            permit = self.notification_sender().reserve() => permit.ok(),
        }
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

