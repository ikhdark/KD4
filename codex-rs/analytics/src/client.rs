use crate::events::AppServerRpcTransport;
use crate::events::TrackEventRequest;
use crate::events::TrackEventsRequest;
use crate::events::current_runtime_metadata;
use crate::facts::AnalyticsFact;
use crate::facts::AnalyticsJsonRpcError;
use crate::facts::AppInvocation;
use crate::facts::AppMentionedInput;
use crate::facts::AppUsedInput;

use crate::facts::CustomAnalyticsFact;
use crate::facts::HookRunFact;
use crate::facts::HookRunInput;
use crate::facts::PluginInstallFailedInput;
use crate::facts::PluginInstallRequested;
use crate::facts::PluginInstallRequestedInput;
use crate::facts::PluginInstallSource;
use crate::facts::PluginState;
use crate::facts::PluginStateChangedInput;
use crate::facts::SkillInvocation;
use crate::facts::SkillInvokedInput;
use crate::facts::SubAgentThreadStartedInput;
use crate::facts::TrackEventsContext;
use crate::facts::TurnCodexErrorFact;
use crate::facts::TurnDeliveryFact;
use crate::facts::TurnProfileFact;
use crate::facts::TurnResolvedConfigFact;
use crate::facts::TurnTokenUsageFact;
use crate::reducer::AnalyticsReducer;
use crate::reducer::analytics_tool_item;
use crate::reducer::tracked_tool_item_id;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadItem;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::create_client_pool;
use codex_plugin::PluginId;
use codex_plugin::PluginTelemetryMetadata;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;

const ANALYTICS_EVENTS_QUEUE_SIZE: usize = 256;
const ANALYTICS_EVENTS_TIMEOUT: Duration = Duration::from_secs(10);
const ANALYTICS_EVENT_DEDUPE_MAX_KEYS: usize = 4096;
const ANALYTICS_EVENTS_BATCH_SIZE: usize = 32;

type UsageEventKey = (String, String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AnalyticsFailureLogMetadata {
    status: u16,
    declared_body_bytes: Option<u64>,
}

fn analytics_failure_log_metadata(
    status: u16,
    declared_body_bytes: Option<u64>,
) -> AnalyticsFailureLogMetadata {
    AnalyticsFailureLogMetadata {
        status,
        declared_body_bytes,
    }
}

#[derive(Clone)]
pub(crate) struct AnalyticsEventsQueue {
    pub(crate) auth_manager: Option<Arc<AuthManager>>,
    pub(crate) loss_epoch: Arc<AtomicU64>,
    pub(crate) sender: mpsc::Sender<AnalyticsFact>,
    pub(crate) app_used_emitted_keys: Arc<Mutex<HashSet<UsageEventKey>>>,
    pub(crate) plugin_used_emitted_keys: Arc<Mutex<HashSet<UsageEventKey>>>,
}

struct AnalyticsPermit<'a> {
    permit: mpsc::Permit<'a, AnalyticsFact>,
    epoch: Option<u64>,
}

impl AnalyticsPermit<'_> {
    fn send(self, fact: AnalyticsFact) {
        self.permit.send(bind_analytics_fact(self.epoch, fact));
    }
}

fn bind_analytics_fact(epoch: Option<u64>, fact: AnalyticsFact) -> AnalyticsFact {
    match epoch {
        Some(epoch) => AnalyticsFact::AuthBound { epoch, fact: Box::new(fact) },
        None => fact,
    }
}

struct AnalyticsDispatchGuard { auth_manager: Arc<AuthManager>, epoch: u64 }
impl codex_http_client::RequestDispatchGuard for AnalyticsDispatchGuard {
    fn admit(&self, dispatch: &mut dyn FnMut()) -> Result<(), String> {
        self.auth_manager.with_policy_auth_epoch(self.epoch, dispatch)
            .map_err(|_| "analytics authentication changed before dispatch".into())
    }
}

async fn ingest_bound_fact(
    input: AnalyticsFact, reducer: &mut AnalyticsReducer, auth: &AuthManager,
    reducer_epoch: &mut Option<u64>, events: &mut Vec<TrackEventRequest>,
) {
    let AnalyticsFact::AuthBound { epoch, fact } = input else { return; };
    let Ok((current_epoch, _)) = auth.policy_auth_snapshot() else { return; };
    if *reducer_epoch != Some(current_epoch) {
        reducer.reset_for_auth_transition();
        events.clear();
        *reducer_epoch = Some(current_epoch);
    }
    if epoch == current_epoch { reducer.ingest(*fact, events).await; }
}

#[derive(Clone)]
pub struct AnalyticsEventsClient {
    queue: Option<AnalyticsEventsQueue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AnalyticsEventsDestination {
    Http {
        url: String,
    },
    #[cfg(debug_assertions)]
    CaptureFile {
        path: PathBuf,
    },
}

impl AnalyticsEventsDestination {
    fn from_base_url(base_url: String) -> Self {
        let capture_file = analytics_capture_file_from_env();
        Self::from_base_url_and_capture_file(base_url, capture_file)
    }

    fn from_base_url_and_capture_file(base_url: String, capture_file: Option<PathBuf>) -> Self {
        #[cfg(debug_assertions)]
        if let Some(path) = capture_file {
            if let Err(err) = crate::analytics_capture::initialize(&path) {
                tracing::error!(
                    path = %path.display(),
                    "failed to initialize analytics event capture; network delivery remains disabled: {err}"
                );
            }
            tracing::warn!(
                path = %path.display(),
                "analytics event capture enabled; network delivery is disabled"
            );
            return Self::CaptureFile { path };
        }

        #[cfg(not(debug_assertions))]
        let _ = capture_file;

        let base_url = base_url.trim_end_matches('/');
        Self::Http {
            url: format!("{base_url}/codex/analytics-events/events"),
        }
    }
}

fn analytics_capture_file_from_env() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        std::env::var_os(crate::analytics_capture::ANALYTICS_EVENTS_CAPTURE_FILE_ENV_VAR)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    #[cfg(not(debug_assertions))]
    None
}

impl AnalyticsEventsQueue {
    fn new(
        auth_manager: Arc<AuthManager>,
        destination: AnalyticsEventsDestination,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel(ANALYTICS_EVENTS_QUEUE_SIZE);
        let http_clients = create_client_pool(http_client_factory, ClientRouteClass::Api);
        let loss_epoch = Arc::new(AtomicU64::new(0));
        let worker_loss_epoch = Arc::clone(&loss_epoch);
        let queue_auth_manager = Arc::clone(&auth_manager);
        // Keep reduction responsive while a single ordered HTTP owner is slow.
        // Bound queued dispatch batches to the original fact queue's scale.
        let (dispatch, mut dispatches) = mpsc::channel::<(u64, Vec<TrackEventRequest>)>(
            ANALYTICS_EVENTS_QUEUE_SIZE / ANALYTICS_EVENTS_BATCH_SIZE,
        );
        let dispatch_auth = Arc::clone(&auth_manager);
        tokio::spawn(async move {
            while let Some((epoch, events)) = dispatches.recv().await {
                send_track_events(&dispatch_auth, epoch, &destination, &http_clients, events).await;
            }
        });
        tokio::spawn(async move {
            let mut reducer = AnalyticsReducer::default();
            let mut reducer_epoch = None;
            while let Some(input) = receiver.recv().await {
                let mut events = Vec::new();
                reducer.observe_delivery_loss(worker_loss_epoch.load(Ordering::Acquire));
                ingest_bound_fact(input, &mut reducer, &auth_manager, &mut reducer_epoch, &mut events).await;
                for _ in 1..ANALYTICS_EVENTS_BATCH_SIZE {
                    let Ok(input) = receiver.try_recv() else {
                        break;
                    };
                    reducer.observe_delivery_loss(worker_loss_epoch.load(Ordering::Acquire));
                    ingest_bound_fact(input, &mut reducer, &auth_manager, &mut reducer_epoch, &mut events).await;
                }
                // Read the loss epoch before the emptiness check. A loss that
                // follows an earlier-enqueued fact must not be absorbed by this
                // drain and then let that fact's turn start without the gap.
                let drained_epoch = worker_loss_epoch.load(Ordering::Acquire);
                if receiver.is_empty() {
                    reducer.delivery_queue_drained(drained_epoch);
                }
                if let Some(epoch) = reducer_epoch
                    && !events.is_empty()
                    && dispatch.try_send((epoch, events)).is_err()
                {
                    let _ = worker_loss_epoch.try_update(Ordering::AcqRel, Ordering::Acquire,
                        |epoch| Some(epoch.saturating_add(1)));
                    tracing::warn!("dropping analytics dispatch: queue is full or closed");
                }
            }
        });
        Self {
            auth_manager: Some(queue_auth_manager),
            sender,
            loss_epoch,
            app_used_emitted_keys: Arc::new(Mutex::new(HashSet::new())),
            plugin_used_emitted_keys: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn try_send(&self, input: AnalyticsFact) {
        let epoch = match self.admission_epoch() { Ok(epoch) => epoch, Err(()) => return };
        if self.sender.try_send(bind_analytics_fact(epoch, input)).is_err() {
            self.record_delivery_loss();
            tracing::warn!("dropping analytics events: queue is full");
        }
    }

    fn admission_epoch(&self) -> Result<Option<u64>, ()> {
        self.auth_manager.as_ref().map(|auth| auth.policy_auth_snapshot()
            .map(|(epoch, _)| epoch).map_err(|_| ())).transpose()
    }

    fn try_reserve(&self) -> Option<AnalyticsPermit<'_>> {
        let epoch = self.admission_epoch().ok()?;
        self.sender
            .try_reserve()
            .map(|permit| AnalyticsPermit { permit, epoch })
            .map_err(|err| {
                self.record_delivery_loss();
                tracing::warn!("dropping analytics events: {err}");
            })
            .ok()
    }

    fn record_delivery_loss(&self) {
        let _ = self
            .loss_epoch
            .try_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                Some(epoch.saturating_add(1))
            });
    }

    fn try_reserve_usage(
        &self,
        emitted: &Mutex<HashSet<UsageEventKey>>,
        key: Option<UsageEventKey>,
    ) -> Option<AnalyticsPermit<'_>> {
        let Some(mut key) = key else { return self.try_reserve(); };
        let epoch = self.admission_epoch().ok()?;
        if let Some(epoch) = epoch {
            key.0 = format!("{epoch}:{}", key.0);
        }
        let mut emitted = emitted.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if emitted.contains(&key) {
            return None;
        }
        // Admission and deduplication are one synchronous operation. A
        // duplicate at capacity is not delivery loss, and a rejected first
        // emission must remain retryable.
        let permit = self.sender.try_reserve().map_err(|_| self.record_delivery_loss()).ok()?;
        let permit = AnalyticsPermit { permit, epoch };
        if emitted.len() >= ANALYTICS_EVENT_DEDUPE_MAX_KEYS {
            emitted.clear();
        }
        emitted.insert(key);
        Some(permit)
    }
}

impl AnalyticsEventsClient {
    pub fn new(
        auth_manager: Arc<AuthManager>,
        base_url: String,
        analytics_enabled: Option<bool>,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            queue: (analytics_enabled != Some(false)).then(|| {
                let destination = AnalyticsEventsDestination::from_base_url(base_url);
                AnalyticsEventsQueue::new(
                    Arc::clone(&auth_manager),
                    destination,
                    http_client_factory,
                )
            }),
        }
    }

    pub fn disabled() -> Self {
        Self { queue: None }
    }

    pub fn track_skill_invocations(
        &self,
        tracking: TrackEventsContext,
        invocations: Vec<SkillInvocation>,
    ) {
        if invocations.is_empty() {
            return;
        }
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::SkillInvoked(
            SkillInvokedInput {
                tracking,
                invocations,
            },
        )));
    }

    pub fn track_initialize(
        &self,
        connection_id: u64,
        params: InitializeParams,
        product_client_id: String,
        rpc_transport: AppServerRpcTransport,
    ) {
        let Some(permit) = self
            .queue
            .as_ref()
            .and_then(AnalyticsEventsQueue::try_reserve)
        else {
            return;
        };
        permit.send(AnalyticsFact::Initialize {
            connection_id,
            params,
            product_client_id,
            runtime: current_runtime_metadata(),
            rpc_transport,
        });
    }

    pub fn track_subagent_thread_started(&self, input: SubAgentThreadStartedInput) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::SubAgentThreadStarted(input),
        ));
    }

    pub fn track_app_mentioned(&self, tracking: TrackEventsContext, mentions: Vec<AppInvocation>) {
        if mentions.is_empty() {
            return;
        }
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::AppMentioned(
            AppMentionedInput { tracking, mentions },
        )));
    }

    pub fn track_request(
        &self,
        connection_id: u64,
        request_id: RequestId,
        request: &ClientRequest,
    ) {
        if !matches!(
            request,
            ClientRequest::TurnStart { .. } | ClientRequest::TurnSteer { .. }
        ) {
            return;
        }
        let Some(permit) = self
            .queue
            .as_ref()
            .and_then(AnalyticsEventsQueue::try_reserve)
        else {
            return;
        };
        permit.send(AnalyticsFact::TurnRequest {
            connection_id,
            request_id,
            request: match request {
                ClientRequest::TurnStart { params, .. } => crate::facts::TurnRequestFact::Start {
                    thread_id: params.thread_id.clone(),
                    num_input_images: crate::reducer::num_input_images(&params.input),
                },
                ClientRequest::TurnSteer { params, .. } => crate::facts::TurnRequestFact::Steer {
                    thread_id: params.thread_id.clone(), expected_turn_id: params.expected_turn_id.clone(),
                    num_input_images: crate::reducer::num_input_images(&params.input),
                },
                _ => unreachable!("request selection above"),
            },
        });
    }

    pub fn track_app_used(&self, tracking: TrackEventsContext, app: AppInvocation) {
        let Some(queue) = self.queue.as_ref() else {
            return;
        };
        let key = app.connector_id.as_ref().map(|id|
            (tracking.thread_id.clone(), tracking.turn_id.clone(), id.clone()));
        let Some(permit) = queue.try_reserve_usage(&queue.app_used_emitted_keys, key) else { return; };
        permit.send(AnalyticsFact::Custom(CustomAnalyticsFact::AppUsed(
            AppUsedInput { tracking, app },
        )));
    }

    pub fn track_hook_run(&self, tracking: TrackEventsContext, hook: HookRunFact) {
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::HookRun(
            HookRunInput { tracking, hook },
        )));
    }

    pub fn track_plugin_used(&self, tracking: TrackEventsContext, plugin: PluginTelemetryMetadata) {
        let Some(queue) = self.queue.as_ref() else {
            return;
        };
        let key = plugin.plugin_id.as_ref().map(PluginId::as_key)
            .or_else(|| plugin.remote_plugin_id.clone())
            .map(|id| (tracking.thread_id.clone(), tracking.turn_id.clone(), id));
        let Some(permit) = queue.try_reserve_usage(&queue.plugin_used_emitted_keys, key) else { return; };
        permit.send(AnalyticsFact::Custom(CustomAnalyticsFact::PluginUsed(
            crate::facts::PluginUsedInput { tracking, plugin },
        )));
    }

    pub fn track_plugin_install_requested(
        &self,
        tracking: TrackEventsContext,
        request: PluginInstallRequested,
    ) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::PluginInstallRequested(PluginInstallRequestedInput {
                tracking,
                request,
            }),
        ));
    }

    pub fn track_compaction(&self, event: crate::facts::CodexCompactionEvent) {
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::Compaction(
            Box::new(event),
        )));
    }



    pub fn track_turn_resolved_config(&self, fact: TurnResolvedConfigFact) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::TurnResolvedConfig(Box::new(fact)),
        ));
    }

    pub fn track_turn_token_usage(&self, fact: TurnTokenUsageFact) {
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::TurnTokenUsage(
            Box::new(fact),
        )));
    }

    pub fn track_turn_profile(&self, fact: TurnProfileFact) {
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::TurnProfile(
            Box::new(fact),
        )));
    }

    pub fn track_turn_delivery(&self, fact: TurnDeliveryFact) {
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::TurnDelivery(
            Box::new(fact),
        )));
    }

    pub fn track_turn_codex_error(&self, fact: TurnCodexErrorFact) {
        self.record_fact(AnalyticsFact::Custom(CustomAnalyticsFact::TurnCodexError(
            Box::new(fact),
        )));
    }

    pub fn track_plugin_installed(&self, plugin: PluginTelemetryMetadata) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::PluginStateChanged(PluginStateChangedInput {
                plugin,
                state: PluginState::Installed,
            }),
        ));
    }

    pub fn track_plugin_install_failed(
        &self,
        plugin: PluginTelemetryMetadata,
        source: PluginInstallSource,
        error_type: String,
    ) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::PluginInstallFailed(PluginInstallFailedInput {
                plugin,
                source,
                error_type,
            }),
        ));
    }



    pub fn track_plugin_uninstalled(&self, plugin: PluginTelemetryMetadata) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::PluginStateChanged(PluginStateChangedInput {
                plugin,
                state: PluginState::Uninstalled,
            }),
        ));
    }

    pub fn track_plugin_enabled(&self, plugin: PluginTelemetryMetadata) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::PluginStateChanged(PluginStateChangedInput {
                plugin,
                state: PluginState::Enabled,
            }),
        ));
    }

    pub fn track_plugin_disabled(&self, plugin: PluginTelemetryMetadata) {
        self.record_fact(AnalyticsFact::Custom(
            CustomAnalyticsFact::PluginStateChanged(PluginStateChangedInput {
                plugin,
                state: PluginState::Disabled,
            }),
        ));
    }

    pub(crate) fn record_fact(&self, input: AnalyticsFact) {
        if let Some(queue) = self.queue.as_ref() {
            queue.try_send(input);
        }
    }

    pub fn track_connection_closed(&self, connection_id: u64) {
        self.record_fact(AnalyticsFact::ConnectionClosed { connection_id });
    }

    pub fn track_response(
        &self,
        connection_id: u64,
        request_id: RequestId,
        response: ClientResponsePayload,
    ) {
        self.track_response_inner(
            connection_id,
            request_id,
            response,
            /*thread_originator*/ None,
        );
    }

    pub fn track_response_with_thread_originator(
        &self,
        connection_id: u64,
        request_id: RequestId,
        response: ClientResponsePayload,
        thread_originator: String,
    ) {
        self.track_response_inner(connection_id, request_id, response, Some(thread_originator));
    }

    fn track_response_inner(
        &self,
        connection_id: u64,
        request_id: RequestId,
        mut response: ClientResponsePayload,
        thread_originator: Option<String>,
    ) {
        if !matches!(
            response,
            ClientResponsePayload::ThreadStart(_)
                | ClientResponsePayload::ThreadResume(_)
                | ClientResponsePayload::ThreadFork(_)
                | ClientResponsePayload::TurnStart(_)
                | ClientResponsePayload::TurnSteer(_)
        ) {
            return;
        }
        // Initialization analytics consumes metadata, never historic turns.
        match &mut response {
            ClientResponsePayload::ThreadStart(response) => response.thread.turns.clear(),
            ClientResponsePayload::ThreadResume(response) => response.thread.turns.clear(),
            ClientResponsePayload::ThreadFork(response) => response.thread.turns.clear(),
            _ => {}
        }
        self.record_fact(AnalyticsFact::ClientResponse {
            connection_id,
            request_id,
            response: Box::new(response),
            thread_originator,
        });
    }

    pub fn track_error_response(
        &self,
        connection_id: u64,
        request_id: RequestId,
        error_type: Option<AnalyticsJsonRpcError>,
    ) {
        self.record_fact(AnalyticsFact::ErrorResponse {
            connection_id,
            request_id,
            error_type,
        });
    }

    pub fn track_server_request(&self, connection_id: u64, request: &ServerRequest) {
        let Some(queue) = self.queue.as_ref() else {
            return;
        };
        if !matches!(
            request,
            ServerRequest::CommandExecutionRequestApproval { .. }
                | ServerRequest::FileChangeRequestApproval { .. }
                | ServerRequest::PermissionsRequestApproval { .. }
        ) {
            return;
        }
        let Some(permit) = queue.try_reserve() else {
            return;
        };
        permit.send(AnalyticsFact::ServerRequest {
            connection_id,
            request: Box::new(request.clone()),
        });
    }

    pub fn track_server_response(
        &self,
        completed_at_ms: u64,
        request: &ServerRequest,
        result: &serde_json::Value,
    ) {
        // Only these responses are consumed by the reducer. In particular, do
        // not clone and decode large dynamic-tool results on their delivery path.
        // Permissions telemetry uses the separately resolved effective response.
        if !matches!(
            request,
            ServerRequest::CommandExecutionRequestApproval { .. }
                | ServerRequest::FileChangeRequestApproval { .. }
        ) {
            return;
        }
        let Some(permit) = self
            .queue
            .as_ref()
            .and_then(AnalyticsEventsQueue::try_reserve)
        else {
            return;
        };
        let Ok(response) = request.response_from_result(result.clone()) else {
            return;
        };
        permit.send(AnalyticsFact::ServerResponse {
            completed_at_ms,
            response: Box::new(response),
        });
    }

    pub fn track_effective_permissions_approval_response(
        &self,
        completed_at_ms: u64,
        request_id: RequestId,
        response: RequestPermissionsResponse,
    ) {
        self.record_fact(AnalyticsFact::EffectivePermissionsApprovalResponse {
            completed_at_ms,
            request_id,
            response: Box::new(response),
        });
    }

    pub fn track_server_request_aborted(&self, completed_at_ms: u64, request_id: RequestId) {
        self.record_fact(AnalyticsFact::ServerRequestAborted {
            completed_at_ms,
            request_id,
        });
    }

    pub fn track_notification(&self, notification: &ServerNotification) {
        let Some(queue) = self.queue.as_ref() else {
            return;
        };
        let relevant = match notification {
            ServerNotification::ItemStarted(notification) => {
                tracked_tool_item_id(&notification.item).is_some()
            }
            ServerNotification::ItemCompleted(notification) => {
                tracked_tool_item_id(&notification.item).is_some()
                    || matches!(notification.item, ThreadItem::SubAgentActivity { .. })
            }
            ServerNotification::TurnStarted(_)
            | ServerNotification::TurnCompleted(_)
            | ServerNotification::TurnDiffUpdated(_)
            | ServerNotification::ThreadClosed(_)
            | ServerNotification::ThreadArchived(_)
            | ServerNotification::ThreadUnarchived(_) => true,
            _ => false,
        };
        if !relevant {
            return;
        }
        let Some(permit) = queue.try_reserve() else {
            return;
        };
        let notification = match notification {
            ServerNotification::ItemStarted(started) => {
                let Some(item) = analytics_tool_item(&started.item) else {
                    return;
                };
                ServerNotification::ItemStarted(ItemStartedNotification {
                    item,
                    thread_id: started.thread_id.clone(),
                    turn_id: started.turn_id.clone(),
                    started_at_ms: started.started_at_ms,
                })
            }
            ServerNotification::ItemCompleted(completed) => {
                let item = match &completed.item {
                    item @ ThreadItem::SubAgentActivity { .. } => item.clone(),
                    item => {
                        let Some(item) = analytics_tool_item(item) else {
                            return;
                        };
                        item
                    }
                };
                ServerNotification::ItemCompleted(ItemCompletedNotification {
                    item,
                    thread_id: completed.thread_id.clone(),
                    turn_id: completed.turn_id.clone(),
                    completed_at_ms: completed.completed_at_ms,
                })
            }
            notification => notification.clone(),
        };
        permit.send(AnalyticsFact::Notification(Box::new(notification)));
    }
}

async fn send_track_events(
    auth_manager: &Arc<AuthManager>,
    epoch: u64,
    destination: &AnalyticsEventsDestination,
    http_clients: &RouteAwareClientPool,
    events: Vec<TrackEventRequest>,
) {
    if events.is_empty() {
        return;
    }

    let Some(auth) = auth_manager.auth().await else {
        return;
    };
    if !auth.uses_codex_backend() {
        return;
    }
    if auth_manager.policy_auth_snapshot().ok().map(|snapshot| snapshot.0) != Some(epoch) {
        return;
    }

    for events in track_event_request_batches(events) {
        codex_http_client::with_dispatch_guard(Some(Arc::new(AnalyticsDispatchGuard {
            auth_manager: Arc::clone(auth_manager), epoch,
        })), send_track_events_request(&auth, destination, http_clients, events)).await;
    }
}

fn track_event_request_batches(events: Vec<TrackEventRequest>) -> Vec<Vec<TrackEventRequest>> {
    let mut batches = Vec::new();
    let mut current_batch = Vec::new();

    for event in events {
        if event.should_send_in_isolated_request() {
            if !current_batch.is_empty() {
                batches.push(current_batch);
                current_batch = Vec::new();
            }
            batches.push(vec![event]);
        } else {
            current_batch.push(event);
            if current_batch.len() == ANALYTICS_EVENTS_BATCH_SIZE {
                batches.push(std::mem::take(&mut current_batch));
            }
        }
    }

    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    batches
}

async fn send_track_events_request(
    auth: &CodexAuth,
    destination: &AnalyticsEventsDestination,
    http_clients: &RouteAwareClientPool,
    events: Vec<TrackEventRequest>,
) {
    if events.is_empty() {
        return;
    }

    let payload = TrackEventsRequest { events };

    #[cfg(debug_assertions)]
    if capture_track_events_request(destination, &payload) {
        return;
    }

    let url = match destination {
        AnalyticsEventsDestination::Http { url } => url,
        #[cfg(debug_assertions)]
        AnalyticsEventsDestination::CaptureFile { .. } => return,
    };
    let response = http_clients
        .post(url)
        .timeout(ANALYTICS_EVENTS_TIMEOUT)
        .headers(codex_model_provider::auth_provider_from_auth(auth).to_auth_headers())
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            let status = response.status();
            let metadata =
                analytics_failure_log_metadata(status.as_u16(), response.content_length());
            tracing::warn!(
                status = metadata.status,
                declared_body_bytes = metadata.declared_body_bytes,
                "events request failed"
            );
        }
        Err(err) => {
            tracing::warn!("failed to send events request: {err}");
        }
    }
}

#[cfg(debug_assertions)]
fn capture_track_events_request(
    destination: &AnalyticsEventsDestination,
    payload: &TrackEventsRequest,
) -> bool {
    let AnalyticsEventsDestination::CaptureFile { path } = destination else {
        return false;
    };

    if let Err(err) = crate::analytics_capture::append_payload(path, payload) {
        tracing::error!(
            path = %path.display(),
            "failed to capture analytics events; network delivery remains disabled: {err}"
        );
    }
    true
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
