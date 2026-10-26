use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use arc_swap::ArcSwap;
use codex_exec_server_protocol::JSONRPCNotification;
use codex_http_client::HttpError;
use codex_utils_absolute_path::is_windows_absolute_path;
use codex_utils_path_uri::PathConvention;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use tokio::time::Instant;
use tokio::time::timeout;
use tracing::Instrument;
use tracing::debug;

use crate::ProcessId;
use crate::client_api::ExecServerClientConnectOptions;
use crate::client_api::ExecServerTransportParams;
use crate::client_api::HttpClient;
use crate::client_api::RemoteExecServerConnectArgs;
use crate::client_api::StdioExecServerConnectArgs;
use crate::client_transport::ExecServerReconnectStrategy;
use crate::connection::JsonRpcConnection;
use crate::process::ExecProcessEvent;
use crate::process::ExecProcessEventLog;
use crate::process::ExecProcessEventReceiver;
use crate::protocol::ENVIRONMENT_INFO_METHOD;
use crate::protocol::EXEC_CLOSED_METHOD;
use crate::protocol::EXEC_EXITED_METHOD;
use crate::protocol::EXEC_FAILED_METHOD;
use crate::protocol::EXEC_METHOD;
use crate::protocol::EXEC_OUTPUT_DELTA_METHOD;
use crate::protocol::EXEC_READ_METHOD;
use crate::protocol::EXEC_SIGNAL_METHOD;
use crate::protocol::EXEC_TERMINATE_METHOD;
use crate::protocol::EXEC_WRITE_METHOD;
use crate::protocol::EnvironmentInfo;
use crate::protocol::ExecClosedNotification;
use crate::protocol::ExecExitedNotification;
use crate::protocol::ExecFailedNotification;
use crate::protocol::ExecOutputDeltaNotification;
use crate::protocol::ExecParams;
use crate::protocol::ExecResponse;
use crate::protocol::FS_CANONICALIZE_METHOD;
use crate::protocol::FS_CLOSE_METHOD;
use crate::protocol::FS_COPY_METHOD;
use crate::protocol::FS_CREATE_DIRECTORY_METHOD;
use crate::protocol::FS_GET_METADATA_METHOD;
use crate::protocol::FS_OPEN_METHOD;
use crate::protocol::FS_READ_BLOCK_METHOD;
use crate::protocol::FS_READ_DIRECTORY_METHOD;
use crate::protocol::FS_READ_FILE_BOUNDED_METHOD;
use crate::protocol::FS_READ_FILE_METHOD;
use crate::protocol::FS_REMOVE_METHOD;
use crate::protocol::FS_WALK_METHOD;
use crate::protocol::FS_WRITE_FILE_METHOD;
use crate::protocol::FsCanonicalizeParams;
use crate::protocol::FsCanonicalizeResponse;
use crate::protocol::FsCloseParams;
use crate::protocol::FsCloseResponse;
use crate::protocol::FsCopyParams;
use crate::protocol::FsCopyResponse;
use crate::protocol::FsCreateDirectoryParams;
use crate::protocol::FsCreateDirectoryResponse;
use crate::protocol::FsGetMetadataParams;
use crate::protocol::FsGetMetadataResponse;
use crate::protocol::FsOpenParams;
use crate::protocol::FsOpenResponse;
use crate::protocol::FsReadBlockParams;
use crate::protocol::FsReadBlockResponse;
use crate::protocol::FsReadDirectoryParams;
use crate::protocol::FsReadDirectoryResponse;
use crate::protocol::FsReadFileBoundedParams;
use crate::protocol::FsReadFileBoundedResponse;
use crate::protocol::FsReadFileParams;
use crate::protocol::FsReadFileResponse;
use crate::protocol::FsRemoveParams;
use crate::protocol::FsRemoveResponse;
use crate::protocol::FsWalkParams;
use crate::protocol::FsWalkResponse;
use crate::protocol::FsWriteFileParams;
use crate::protocol::FsWriteFileResponse;
use crate::protocol::HTTP_REQUEST_BODY_DELTA_METHOD;
use crate::protocol::HttpRequestBodyDeltaNotification;
use crate::protocol::INITIALIZE_METHOD;
use crate::protocol::INITIALIZED_METHOD;
use crate::protocol::InitializeParams;
use crate::protocol::InitializeResponse;
use crate::protocol::ProcessOutputChunk;
use crate::protocol::ProcessSignal;
use crate::protocol::ReadParams;
use crate::protocol::ReadResponse;
use crate::protocol::SignalParams;
use crate::protocol::SignalResponse;
use crate::protocol::TerminateParams;
use crate::protocol::TerminateResponse;
use crate::protocol::WriteParams;
use crate::protocol::WriteResponse;
use crate::rpc::RpcCallError;
use crate::rpc::RpcClient;

pub(crate) mod http_client;
mod deferred_notification;
#[path = "client_recovery.rs"]
pub(crate) mod recovery;
use crate::ProcessCleanupRecord;
use crate::ProcessCleanupStatus;
use crate::ProcessCleanupTicket;
use crate::ProcessExecutionKnowledge;
pub(crate) use recovery::is_retryable_registry_error;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const ENVIRONMENT_INFO_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const PROCESS_START_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_EVENT_CHANNEL_CAPACITY: usize = 256;
const PROCESS_EVENT_RETAINED_BYTES: usize = 1024 * 1024;
const MAX_PENDING_PROCESS_EVENTS: usize = 256;
const MAX_PENDING_PROCESS_EVENT_BYTES: usize = 1024 * 1024;

impl Default for ExecServerClientConnectOptions {
    fn default() -> Self {
        Self {
            client_name: "codex-core".to_string(),
            initialize_timeout: INITIALIZE_TIMEOUT,
            resume_session_id: None,
        }
    }
}

impl From<RemoteExecServerConnectArgs> for ExecServerClientConnectOptions {
    fn from(value: RemoteExecServerConnectArgs) -> Self {
        Self {
            client_name: value.client_name,
            initialize_timeout: value.initialize_timeout,
            resume_session_id: value.resume_session_id,
        }
    }
}

impl From<StdioExecServerConnectArgs> for ExecServerClientConnectOptions {
    fn from(value: StdioExecServerConnectArgs) -> Self {
        Self {
            client_name: value.client_name,
            initialize_timeout: value.initialize_timeout,
            resume_session_id: value.resume_session_id,
        }
    }
}

impl RemoteExecServerConnectArgs {
    pub fn new(websocket_url: String, client_name: String) -> Self {
        Self {
            websocket_url,
            client_name,
            connect_timeout: CONNECT_TIMEOUT,
            initialize_timeout: INITIALIZE_TIMEOUT,
            resume_session_id: None,
        }
    }
}

pub(crate) struct SessionState {
    incarnation: String,
    wake_tx: watch::Sender<u64>,
    events: ExecProcessEventLog,
    ordered_events: StdMutex<OrderedSessionEvents>,
    recoverable: AtomicBool,
    next_write_id: AtomicU64,
}

#[derive(Default)]
struct OrderedSessionEvents {
    last_published_seq: u64,
    exit_published: bool,
    observed_exited: bool,
    last_exit_code: Option<i32>,
    sandbox_denied: bool,
    closed_published: bool,
    // Server-side output, exit, and closed notifications are emitted by
    // different tasks and can reach the client out of order. Keep future events
    // here until all lower sequence numbers have been published.
    pending: BTreeMap<u64, ExecProcessEvent>,
    pending_bytes: usize,
    // A private retained-read validator already owns the bounded response.
    pending_byte_limit: Option<usize>,
    failure: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Session {
    client: ExecServerClient,
    process_id: ProcessId,
    state: Arc<SessionState>,
}

/// Routes and undelivered failures share one lock so cleanup is atomic without
/// cloning either map on each registration or terminal event.
#[derive(Default)]
struct HttpBodyStreams {
    streams: HashMap<String, mpsc::Sender<HttpRequestBodyDeltaNotification>>,
    terminals: HashMap<String, Result<HttpRequestBodyDeltaNotification, String>>,
}

struct UnsupportedFsMethods {
    connection: Weak<RpcClient>,
    methods: HashMap<&'static str, String>,
}

struct Inner {
    instance_id: String,
    cleanup_records: StdMutex<std::collections::VecDeque<ProcessCleanupTicket>>,
    cleanup_records_evicted: AtomicU64,
    file_read_slots: Arc<tokio::sync::Semaphore>,
    // Key negative capability evidence to the transport, not the recoverable client.
    unsupported_fs_methods: StdMutex<Option<UnsupportedFsMethods>>,
    connection: StdMutex<ConnectionState>,
    connection_changed: watch::Sender<()>,
    // The remote transport delivers one shared notification stream for every
    // process on the connection. Keep a local process_id -> session registry so
    // we can turn those connection-global notifications into process wakeups
    // without making notifications the source of truth for output delivery.
    sessions: ArcSwap<HashMap<ProcessId, Arc<SessionState>>>,
    // ArcSwap makes reads cheap on the hot notification path, but writes still
    // need serialization so concurrent register/remove operations do not
    // overwrite each other's copy-on-write updates.
    sessions_write_lock: StdMutex<()>,
    // Streaming HTTP responses are keyed by a client-generated request id
    // because they share the same connection-global notification channel as
    // process output. Keep the routing table local to the client so higher
    // layers can consume body chunks like a normal byte stream.
    http_body_streams: Mutex<HttpBodyStreams>,
    http_body_stream_next_id: AtomicU64,
    session_id: OnceLock<String>,
    environment_info: OnceLock<EnvironmentInfo>,
    environment_info_init: Mutex<()>,
    reconnect_strategy: Option<ExecServerReconnectStrategy>,
}

struct ConnectionState {
    status: ConnectionStatus,
    active_process_starts: usize,
}

enum ConnectionStatus {
    Connected(Arc<RpcClient>),
    Recovering,
    Failed(String),
}

#[derive(Clone, Copy)]
enum RecoveryPolicy {
    Wait,
    FailFast,
}

#[derive(Clone)]
pub struct ExecServerClient {
    inner: Arc<Inner>,
    recovery_policy: RecoveryPolicy,
}

struct ActiveProcessStart {
    inner: Arc<Inner>,
}

impl Drop for ActiveProcessStart {
    fn drop(&mut self) {
        self.inner.finish_process_start();
    }
}

type ConnectionResult = Result<ExecServerClient, Arc<ExecServerError>>;
type ConnectionAttempt = OnceCell<ConnectionResult>;

#[derive(Clone)]
pub(crate) struct LazyRemoteExecServerClient {
    bound_client: Option<ExecServerClient>,
    transport_params: ExecServerTransportParams,
    recovery_policy: RecoveryPolicy,
    // Saves the first startup result so callers share it. Retryable failures reconnect.
    startup: Arc<ConnectionAttempt>,
    // The latest successful client, replaced whenever reconnecting succeeds.
    current_client: Arc<StdMutex<Option<ExecServerClient>>>,
    reconnect: Arc<StdMutex<Option<Arc<ConnectionAttempt>>>>,
}

impl LazyRemoteExecServerClient {
    pub(crate) fn new(transport_params: ExecServerTransportParams) -> Self {
        Self {
            bound_client: None,
            transport_params,
            recovery_policy: RecoveryPolicy::Wait,
            startup: Arc::new(ConnectionAttempt::new()),
            current_client: Arc::new(StdMutex::new(None)),
            reconnect: Arc::new(StdMutex::new(None)),
        }
    }

    pub(crate) fn start_connecting(&self) -> Option<AbortOnDropHandle<()>> {
        // Stdio starts a process, so keep it lazy until the environment is used.
        if matches!(
            self.transport_params,
            ExecServerTransportParams::StdioCommand { .. }
        ) {
            return None;
        }
        let client = self.clone();
        Some(AbortOnDropHandle::new(tokio::spawn(async move {
            if let Err(error) = client.wait_until_ready().await {
                debug!(%error, "exec-server environment startup failed");
            }
        })))
    }

    pub(crate) fn startup_finished(&self) -> bool {
        self.startup.get().is_some()
    }

    pub(crate) fn readiness_result(&self) -> Option<Result<(), ExecServerError>> {
        if let Some(client) = self.cached_client() {
            return client.readiness_result();
        }
        self.startup.get().and_then(|result| match result {
            Ok(client) => client.readiness_result(),
            Err(error) => Some(Err(ExecServerError::ConnectionAttempt(Arc::clone(error)))),
        })
    }

    pub(crate) fn fail_fast(&self) -> Self {
        Self {
            recovery_policy: RecoveryPolicy::FailFast,
            ..self.clone()
        }
    }

    /// A captured capability can recover its logical session, never replace it.
    pub(crate) fn bound(&self, client: ExecServerClient) -> Self {
        Self {
            bound_client: Some(client),
            ..self.clone()
        }
    }

    pub(crate) async fn wait_until_ready(&self) -> Result<(), ExecServerError> {
        self.initial_client().await.map(drop)
    }

    pub(crate) async fn get(&self) -> Result<ExecServerClient, ExecServerError> {
        if let Some(client) = &self.bound_client {
            if matches!(self.recovery_policy, RecoveryPolicy::FailFast) {
                return client.fail_fast();
            }
            client.rpc_client().await?;
            return Ok(client.clone());
        }
        if matches!(self.recovery_policy, RecoveryPolicy::FailFast) {
            let client = match self.cached_client() {
                Some(client) => client,
                None => match self.startup.get() {
                    Some(Ok(client)) => client.clone(),
                    Some(Err(error)) => {
                        return Err(ExecServerError::ConnectionAttempt(Arc::clone(error)));
                    }
                    None => {
                        return Err(ExecServerError::Disconnected(
                            "exec-server environment is not ready".to_string(),
                        ));
                    }
                },
            };
            return client.fail_fast();
        }
        if let Some(client) = self.connected_client() {
            return Ok(client);
        }

        let Some(cached_client) = self.cached_client() else {
            let client = self.initial_client().await?;
            if !client.is_disconnected() || !self.can_reconnect() {
                return Ok(client);
            }
            return self.reconnect().await;
        };

        if !self.can_reconnect() {
            return Ok(cached_client);
        }

        self.reconnect().await
    }

    async fn initial_client(&self) -> Result<ExecServerClient, ExecServerError> {
        // The first caller starts the work; every other caller waits for that same result.
        // A transient failure remains recorded for observability, but recoverable transports may
        // use the reconnect lane for later attempts instead of burning the environment forever.
        if let Some(Err(error)) = self.startup.get()
            && self.can_reconnect()
            && recovery::is_retryable_recovery_error(error)
        {
            return Box::pin(self.reconnect()).await;
        }
        let result = self
            .startup
            .get_or_init(|| connect_once(self.transport_params.clone()))
            .await;
        match result {
            Ok(client) => {
                let mut current_client = self
                    .current_client
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if current_client.is_none() {
                    *current_client = Some(client.clone());
                }
                Ok(client.clone())
            }
            Err(error) => Err(ExecServerError::ConnectionAttempt(Arc::clone(error))),
        }
    }

    async fn reconnect(&self) -> Result<ExecServerClient, ExecServerError> {
        // Callers handling the same outage share one reconnect attempt.
        let attempt = {
            let mut reconnect = self
                .reconnect
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(client) = self.connected_client() {
                return Ok(client);
            }
            reconnect
                .get_or_insert_with(|| Arc::new(ConnectionAttempt::new()))
                .clone()
        };
        let result = attempt
            .get_or_init(|| async {
                let result = connect_once(self.transport_params.clone()).await;
                if let Ok(client) = &result {
                    *self
                        .current_client
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(client.clone());
                }
                result
            })
            .await;
        let mut reconnect = self
            .reconnect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Forget only this completed attempt so a later operation can retry after failure.
        if reconnect
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &attempt))
        {
            *reconnect = None;
        }
        result.clone().map_err(ExecServerError::ConnectionAttempt)
    }

    fn connected_client(&self) -> Option<ExecServerClient> {
        self.cached_client()
            .filter(|client| !client.is_disconnected())
    }

    pub(crate) fn cached_client(&self) -> Option<ExecServerClient> {
        if let Some(client) = &self.bound_client {
            return Some(client.clone());
        }
        self.current_client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn can_reconnect(&self) -> bool {
        matches!(
            self.transport_params,
            ExecServerTransportParams::WebSocketUrl { .. }
                | ExecServerTransportParams::NoiseRendezvous { .. }
        )
    }
}

async fn connect_once(transport_params: ExecServerTransportParams) -> ConnectionResult {
    let client = ExecServerClient::connect_for_transport(transport_params)
        .await
        .map_err(Arc::new)?;
    let info = client.environment_info().await.map_err(Arc::new)?;
    validate_windows_environment_info(&info).map_err(Arc::new)?;
    let _ = client.inner.environment_info.set(info);
    Ok(client)
}

fn validate_windows_environment_info(info: &EnvironmentInfo) -> Result<(), ExecServerError> {
    if info
        .operating_system
        .as_deref()
        .is_some_and(|operating_system| operating_system != "windows")
    {
        return Err(ExecServerError::Protocol(format!(
            "remote environment reported unsupported operating system `{}`",
            info.operating_system.as_deref().unwrap_or_default()
        )));
    }
    if !matches!(info.shell.name.as_str(), "powershell" | "pwsh" | "cmd") {
        return Err(ExecServerError::Protocol(format!(
            "remote environment reported unsupported Windows shell `{}`",
            info.shell.name
        )));
    }
    if !windows_shell_path_matches_name(&info.shell.name, &info.shell.path) {
        return Err(ExecServerError::Protocol(format!(
            "remote environment reported shell/path mismatch: Windows shell `{}` cannot use executable `{}`",
            info.shell.name, info.shell.path
        )));
    }
    if let Some(cwd) = &info.cwd
        && cwd.infer_path_convention() != Some(PathConvention::Windows)
    {
        return Err(ExecServerError::Protocol(format!(
            "remote environment reported non-Windows working directory `{cwd}`"
        )));
    }
    Ok(())
}

fn windows_shell_path_matches_name(shell_name: &str, shell_path: &str) -> bool {
    let shell_path = shell_path.trim();
    if shell_path.is_empty() || (shell_path.contains('/') && !is_windows_absolute_path(shell_path))
    {
        return false;
    }

    let executable_name = shell_path.rsplit(['/', '\\']).next().unwrap_or(shell_path);
    match shell_name {
        "powershell" | "pwsh" => matches_ignore_ascii_case(
            executable_name,
            &["powershell", "powershell.exe", "pwsh", "pwsh.exe"],
        ),
        "cmd" => matches_ignore_ascii_case(executable_name, &["cmd", "cmd.exe"]),
        _ => false,
    }
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

impl HttpClient for LazyRemoteExecServerClient {
    fn http_request(
        &self,
        params: crate::HttpRequestParams,
    ) -> BoxFuture<'_, Result<crate::HttpRequestResponse, ExecServerError>> {
        async move { self.get().await?.http_request(params).await }.boxed()
    }

    fn http_request_stream(
        &self,
        params: crate::HttpRequestParams,
    ) -> BoxFuture<
        '_,
        Result<(crate::HttpRequestResponse, crate::HttpResponseBodyStream), ExecServerError>,
    > {
        async move { self.get().await?.http_request_stream(params).await }.boxed()
    }
}

impl LazyRemoteExecServerClient {
    pub(crate) async fn environment_info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        self.get().await?.environment_info().await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecServerError {
    #[error("remote process start was not acknowledged: {source}; {ticket}")]
    ProcessStart {
        #[source]
        source: Box<ExecServerError>,
        ticket: ProcessCleanupTicket,
    },
    #[error("failed to spawn exec-server: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("timed out connecting to exec-server websocket `{url}` after {timeout:?}")]
    WebSocketConnectTimeout { url: String, timeout: Duration },
    #[error("failed to connect to exec-server websocket `{url}`: {source}")]
    WebSocketConnect {
        url: String,
        #[source]
        source: tokio_tungstenite::tungstenite::Error,
    },
    #[error("timed out waiting for exec-server initialize handshake after {timeout:?}")]
    InitializeTimedOut { timeout: Duration },
    #[error("exec-server environment/info timed out after {timeout:?}")]
    EnvironmentInfoTimedOut { timeout: Duration },
    #[error("exec-server process start timed out")]
    ProcessStartTimedOut,
    #[error("exec-server transport closed")]
    Closed,
    #[error("{0}")]
    Disconnected(String),
    #[error("failed to serialize or deserialize exec-server JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP request failed: {0}")]
    HttpRequest(String),
    #[error("exec-server protocol error: {0}")]
    Protocol(String),
    #[error("exec-server rejected request ({code}): {message}")]
    Server {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },
    #[error("environment registry request failed ({status}{code_suffix}): {message}", code_suffix = .code.as_ref().map(|code| format!(", {code}")).unwrap_or_default())]
    EnvironmentRegistryHttp {
        status: http::StatusCode,
        code: Option<String>,
        message: String,
    },
    #[error("environment registry configuration error: {0}")]
    EnvironmentRegistryConfig(String),
    #[error("environment registry authentication error: {0}")]
    EnvironmentRegistryAuth(String),
    #[error("environment registry request failed: {0}")]
    EnvironmentRegistryRequest(#[from] HttpError),
    #[error("exec-server connection attempt failed: {0}")]
    ConnectionAttempt(#[source] Arc<ExecServerError>),
}

impl ExecServerClient {
    /// Last 128 cleanup records, plus the number evicted. Records do not own tasks.
    pub fn process_cleanup_records(&self) -> (Vec<ProcessCleanupRecord>, u64) {
        let records = self
            .inner
            .cleanup_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            records.iter().map(ProcessCleanupTicket::snapshot).collect(),
            self.inner.cleanup_records_evicted.load(Ordering::Relaxed),
        )
    }
    fn fail_fast(&self) -> Result<Self, ExecServerError> {
        self.rpc_client_without_recovery()?;
        Ok(Self {
            inner: Arc::clone(&self.inner),
            recovery_policy: RecoveryPolicy::FailFast,
        })
    }

    fn rpc_client_without_recovery(&self) -> Result<Arc<RpcClient>, ExecServerError> {
        let connection = self
            .inner
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &connection.status {
            ConnectionStatus::Connected(rpc_client) if !rpc_client.is_disconnected() => {
                Ok(Arc::clone(rpc_client))
            }
            ConnectionStatus::Connected(_) | ConnectionStatus::Recovering => Err(
                ExecServerError::Disconnected("exec-server environment is recovering".to_string()),
            ),
            ConnectionStatus::Failed(message) => {
                Err(ExecServerError::Disconnected(message.clone()))
            }
        }
    }

    pub(crate) async fn rpc_client(&self) -> Result<Arc<RpcClient>, ExecServerError> {
        match self.recovery_policy {
            RecoveryPolicy::Wait => self.inner.rpc_client().await,
            RecoveryPolicy::FailFast => self.rpc_client_without_recovery(),
        }
    }

    pub async fn initialize(
        &self,
        options: ExecServerClientConnectOptions,
    ) -> Result<InitializeResponse, ExecServerError> {
        let rpc_client = self.rpc_client().await?;
        self.initialize_rpc(&rpc_client, options).await
    }

    async fn initialize_rpc(
        &self,
        rpc_client: &RpcClient,
        options: ExecServerClientConnectOptions,
    ) -> Result<InitializeResponse, ExecServerError> {
        let ExecServerClientConnectOptions {
            client_name,
            initialize_timeout,
            resume_session_id,
        } = options;

        timeout(initialize_timeout, async {
            let response: InitializeResponse = rpc_client
                .call(
                    INITIALIZE_METHOD,
                    &InitializeParams {
                        client_name,
                        resume_session_id: resume_session_id.clone(),
                    },
                )
                .await?;
            if resume_session_id
                .as_ref()
                .is_some_and(|requested| requested != &response.session_id)
            {
                return Err(ExecServerError::Protocol(format!(
                    "exec-server did not resume requested session {} (received {})",
                    resume_session_id.as_deref().unwrap_or_default(),
                    response.session_id
                )));
            }
            let session_id = self
                .inner
                .session_id
                .get_or_init(|| response.session_id.clone());
            if session_id != &response.session_id {
                return Err(ExecServerError::Protocol(format!(
                    "exec-server initialized an unexpected session {}",
                    response.session_id
                )));
            }
            rpc_client
                .notify(INITIALIZED_METHOD, &serde_json::json!({}))
                .await?;
            Ok(response)
        })
        .await
        .map_err(|_| ExecServerError::InitializeTimedOut {
            timeout: initialize_timeout,
        })?
    }

    pub async fn exec(&self, params: ExecParams) -> Result<ExecResponse, ExecServerError> {
        self.call(EXEC_METHOD, &params).await
    }

    pub async fn environment_info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        if let Some(info) = self.inner.environment_info.get() {
            return Ok(info.clone());
        }
        let _initialization = self.inner.environment_info_init.lock().await;
        if let Some(info) = self.inner.environment_info.get() {
            return Ok(info.clone());
        }
        let rpc_client = self.rpc_client().await?;
        let info = match rpc_client
            .call_with_timeout(ENVIRONMENT_INFO_METHOD, &(), ENVIRONMENT_INFO_TIMEOUT)
            .await
        {
            Err(RpcCallError::TimedOut { timeout, .. }) => {
                return Err(ExecServerError::EnvironmentInfoTimedOut { timeout });
            }
            result => map_rpc_call_result(result)?,
        };
        Ok(self.inner.environment_info.get_or_init(|| info).clone())
    }

    pub(crate) fn cached_environment_info(&self) -> Option<EnvironmentInfo> {
        self.inner.environment_info.get().cloned()
    }

    pub(crate) fn cached_coordination_resource_id(&self) -> Option<String> {
        self.inner.session_id.get()?;
        self.inner.environment_info.get()?.resource_id.clone()
    }

    pub async fn read(&self, params: ReadParams) -> Result<ReadResponse, ExecServerError> {
        self.call(EXEC_READ_METHOD, &params).await
    }

    pub async fn write(
        &self,
        process_id: &ProcessId,
        chunk: Vec<u8>,
        write_id: String,
    ) -> Result<WriteResponse, ExecServerError> {
        self.call(
            EXEC_WRITE_METHOD,
            &WriteParams {
                process_id: process_id.clone(),
                chunk: chunk.into(),
                write_id,
            },
        )
        .await
    }

    pub async fn signal(
        &self,
        process_id: &ProcessId,
        signal: ProcessSignal,
    ) -> Result<(), ExecServerError> {
        let _response: SignalResponse = self
            .call(
                EXEC_SIGNAL_METHOD,
                &SignalParams {
                    process_id: process_id.clone(),
                    signal,
                },
            )
            .await?;
        Ok(())
    }

    pub async fn terminate(
        &self,
        process_id: &ProcessId,
    ) -> Result<TerminateResponse, ExecServerError> {
        self.terminate_before(process_id, Instant::now() + PROCESS_TERMINATION_TIMEOUT)
            .await
    }

    async fn terminate_before(
        &self,
        process_id: &ProcessId,
        deadline: Instant,
    ) -> Result<TerminateResponse, ExecServerError> {
        self.terminate_with_incarnation_before(process_id, None, deadline)
            .await
    }

    async fn terminate_with_incarnation_before(
        &self,
        process_id: &ProcessId,
        incarnation: Option<String>,
        deadline: Instant,
    ) -> Result<TerminateResponse, ExecServerError> {
        self.call_for_cleanup_before(
            EXEC_TERMINATE_METHOD,
            &crate::protocol::OwnedProcessParams {
                incarnation,
                params: TerminateParams {
                    process_id: process_id.clone(),
                },
            },
            deadline,
        )
        .await
    }

    pub async fn fs_read_file(
        &self,
        params: FsReadFileParams,
    ) -> Result<FsReadFileResponse, ExecServerError> {
        self.call(FS_READ_FILE_METHOD, &params).await
    }

    pub async fn fs_read_file_bounded(
        &self,
        params: FsReadFileBoundedParams,
    ) -> Result<FsReadFileBoundedResponse, ExecServerError> {
        self.call_optional_fs(FS_READ_FILE_BOUNDED_METHOD, &params).await
    }

    async fn call_optional_fs<P, T>(
        &self,
        method: &'static str,
        params: &P,
    ) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        // Only cache method absence, never path, permission or transient errors.
        const METHOD_NOT_FOUND: i64 = -32601;
        let rpc_client = self.rpc_client().await?;
        {
            let unsupported = self
                .inner
                .unsupported_fs_methods
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = unsupported.as_ref()
                && cached.connection.ptr_eq(&Arc::downgrade(&rpc_client))
                && let Some(message) = cached.methods.get(method)
            {
                return Err(ExecServerError::Server {
                    code: METHOD_NOT_FOUND,
                    message: message.clone(),
                    data: None,
                });
            }
        }
        let result = self.call_rpc(&rpc_client, method, params).await;
        if let Err(ExecServerError::Server { code, message, .. }) = &result
            && *code == METHOD_NOT_FOUND
        {
            let mut unsupported = self
                .inner
                .unsupported_fs_methods
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cached = unsupported.get_or_insert_with(|| UnsupportedFsMethods {
                connection: Arc::downgrade(&rpc_client),
                methods: HashMap::new(),
            });
            if !cached.connection.ptr_eq(&Arc::downgrade(&rpc_client)) {
                cached.connection = Arc::downgrade(&rpc_client);
                cached.methods.clear();
            }
            cached.methods.insert(method, message.clone());
        }
        result
    }

    pub async fn fs_open(&self, params: FsOpenParams) -> Result<FsOpenResponse, ExecServerError> {
        self.call(FS_OPEN_METHOD, &params).await
    }

    pub(crate) async fn reserve_file_stream(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ExecServerError> {
        Arc::clone(&self.inner.file_read_slots)
            .acquire_owned()
            .await
            .map_err(|_| ExecServerError::Closed)
    }

    pub async fn fs_read_block(
        &self,
        params: FsReadBlockParams,
    ) -> Result<FsReadBlockResponse, ExecServerError> {
        self.call(FS_READ_BLOCK_METHOD, &params).await
    }

    pub async fn fs_close(
        &self,
        params: FsCloseParams,
    ) -> Result<FsCloseResponse, ExecServerError> {
        self.call_for_cleanup(FS_CLOSE_METHOD, &params).await
    }

    pub async fn fs_write_file(
        &self,
        params: FsWriteFileParams,
    ) -> Result<FsWriteFileResponse, ExecServerError> {
        self.call(FS_WRITE_FILE_METHOD, &params).await
    }

    pub async fn fs_mutate_file_if_matches(
        &self,
        params: crate::protocol::FsMutateFileIfMatchesParams,
    ) -> Result<crate::protocol::FsMutateFileIfMatchesResponse, ExecServerError> {
        self.call(crate::protocol::FS_MUTATE_FILE_IF_MATCHES_METHOD, &params)
            .await
    }

    pub async fn fs_create_directory(
        &self,
        params: FsCreateDirectoryParams,
    ) -> Result<FsCreateDirectoryResponse, ExecServerError> {
        self.call(FS_CREATE_DIRECTORY_METHOD, &params).await
    }

    pub async fn fs_get_metadata(
        &self,
        params: FsGetMetadataParams,
    ) -> Result<FsGetMetadataResponse, ExecServerError> {
        self.call(FS_GET_METADATA_METHOD, &params).await
    }

    pub async fn fs_canonicalize(
        &self,
        params: FsCanonicalizeParams,
    ) -> Result<FsCanonicalizeResponse, ExecServerError> {
        self.call(FS_CANONICALIZE_METHOD, &params).await
    }

    pub async fn fs_read_directory(
        &self,
        params: FsReadDirectoryParams,
    ) -> Result<FsReadDirectoryResponse, ExecServerError> {
        self.call(FS_READ_DIRECTORY_METHOD, &params).await
    }

    pub async fn fs_walk(&self, params: FsWalkParams) -> Result<FsWalkResponse, ExecServerError> {
        self.call_optional_fs(FS_WALK_METHOD, &params).await
    }

    pub async fn fs_remove(
        &self,
        params: FsRemoveParams,
    ) -> Result<FsRemoveResponse, ExecServerError> {
        self.call(FS_REMOVE_METHOD, &params).await
    }

    pub async fn fs_copy(&self, params: FsCopyParams) -> Result<FsCopyResponse, ExecServerError> {
        self.call(FS_COPY_METHOD, &params).await
    }


    pub(crate) async fn start_process_before(
        &self,
        params: ExecParams,
        deadline: Instant,
    ) -> Result<Session, ExecServerError> {
        let ticket = ProcessCleanupTicket::new(
            params.process_id.clone(),
            self.inner.session_id.get().cloned(),
        );
        let submitted = Arc::new(AtomicBool::new(false));
        let result = tokio::time::timeout_at(
            deadline,
            self.start_process_inner(params, deadline, ticket.clone(), Arc::clone(&submitted)),
        )
        .await
        .unwrap_or(Err(ExecServerError::ProcessStartTimedOut));
        result.map_err(|source| {
            if submitted.load(Ordering::Acquire)
                && matches!(ticket.snapshot().cleanup, ProcessCleanupStatus::NotRequired)
            {
                ticket.update(
                    ProcessExecutionKnowledge::PossiblyExecuted,
                    ProcessCleanupStatus::Pending,
                );
            }
            self.inner.retain_cleanup(ticket.clone());
            ExecServerError::ProcessStart {
                source: Box::new(source),
                ticket,
            }
        })
    }

    async fn start_process_inner(
        &self,
        params: ExecParams,
        deadline: Instant,
        ticket: ProcessCleanupTicket,
        submitted: Arc<AtomicBool>,
    ) -> Result<Session, ExecServerError> {
        loop {
            let rpc_client = self.rpc_client().await?;
            if Instant::now() >= deadline {
                return Err(ExecServerError::ProcessStartTimedOut);
            }
            if !self.inner.begin_process_start(&rpc_client) {
                continue;
            }

            let process_id = params.process_id.clone();
            let state = Arc::new(SessionState::new(/*recoverable*/ false));
            if let Err(error) = self.inner.insert_session(&process_id, Arc::clone(&state)) {
                self.inner.finish_process_start();
                return Err(error);
            }
            let active_start = ActiveProcessStart {
                inner: Arc::clone(&self.inner),
            };
            let client = self.clone();
            let (mut result_tx, result_rx) = tokio::sync::oneshot::channel();
            let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel::<()>();
            let process_start_task = async move {
                let owned_params = crate::protocol::OwnedProcessParams {
                    incarnation: Some(state.incarnation.clone()),
                    params,
                };
                let start_result = tokio::select! {
                    biased;
                    result = rpc_client.call_tracked::<_, ExecResponse>(
                        crate::protocol::EXEC_OWNED_METHOD,
                        &owned_params,
                        deadline.saturating_duration_since(Instant::now()),
                        &submitted,
                    ) => Some(map_rpc_call_result(result)),
                    _ = result_tx.closed() => None,
                };
                match start_result {
                    None => {
                        drop(active_start);
                        cleanup_process_start(&client, &process_id, &state, &ticket, &submitted)
                            .await;
                    }
                    Some(Ok(_)) => {
                        state.recoverable.store(true, Ordering::Release);
                        let session = Session {
                            client: client.clone(),
                            process_id: process_id.clone(),
                            state: Arc::clone(&state),
                        };
                        let send_failed = result_tx.send(Ok(session)).is_err();
                        // Queuing a result is not caller ownership. Do not keep
                        // the recovery-start barrier while awaiting acceptance.
                        drop(active_start);
                        if send_failed || accepted_rx.await.is_err() {
                            state.recoverable.store(false, Ordering::Release);
                            cleanup_process_start(
                                &client,
                                &process_id,
                                &state,
                                &ticket,
                                &submitted,
                            )
                            .await;
                        }
                    }
                    Some(Err(error)) => {
                        // These refusals precede dispatch/native launch. In
                        // particular, never terminate somebody else's duplicate ID.
                        if matches!(
                            &error,
                            ExecServerError::Server {
                                code: -32601
                                    | -32602
                                    | crate::rpc::PROCESS_START_REFUSED_ERROR_CODE,
                                ..
                            }
                        ) {
                            submitted.store(false, Ordering::Release);
                        }
                        if matches!(&error, ExecServerError::Server { code: -32600, .. }) {
                            // Old peers use this for both duplicate IDs and a
                            // cancellation racing native launch. Neither "never
                            // executed" nor terminating that ID is justified.
                            ticket.update(state.execution_knowledge(), ProcessCleanupStatus::Unconfirmed("ambiguous server refusal; cleanup must not terminate a potentially pre-existing process".into()));
                            client.inner.retain_cleanup(ticket.clone());
                            let _ = result_tx.send(Err(error));
                            drop(active_start);
                            client.inner.remove_session_if(&process_id, &state);
                            return;
                        }
                        if submitted.load(Ordering::Acquire) {
                            ticket
                                .update(state.execution_knowledge(), ProcessCleanupStatus::Pending);
                        }
                        let _ = result_tx.send(Err(error));
                        // Cleanup can require a recovered connection. Only the start
                        // outcome and recoverability belong inside the recovery barrier.
                        drop(active_start);
                        cleanup_process_start(&client, &process_id, &state, &ticket, &submitted)
                            .await;
                    }
                }
            };
            tokio::spawn(process_start_task.in_current_span());
            let result = result_rx.await.map_err(|_| {
                ExecServerError::Protocol("process start task stopped unexpectedly".to_string())
            })?;
            if result.is_ok() {
                let _ = accepted_tx.send(());
            }
            return result;
        }
    }


    pub fn session_id(&self) -> Option<String> {
        self.inner.session_id.get().cloned()
    }

    pub(crate) fn instance_id(&self) -> &str {
        &self.inner.instance_id
    }

    fn is_disconnected(&self) -> bool {
        self.inner.is_failed()
    }

    fn readiness_result(&self) -> Option<Result<(), ExecServerError>> {
        let connection = self
            .inner
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &connection.status {
            ConnectionStatus::Connected(rpc_client) if !rpc_client.is_disconnected() => {
                Some(Ok(()))
            }
            ConnectionStatus::Connected(_) | ConnectionStatus::Recovering => None,
            ConnectionStatus::Failed(message) => {
                Some(Err(ExecServerError::Disconnected(message.clone())))
            }
        }
    }

    pub(crate) async fn connect(
        connection: JsonRpcConnection,
        options: ExecServerClientConnectOptions,
    ) -> Result<Self, ExecServerError> {
        Self::connect_with_recovery(connection, options, /*reconnect_strategy*/ None).await
    }

    pub(crate) async fn connect_with_recovery(
        connection: JsonRpcConnection,
        options: ExecServerClientConnectOptions,
        reconnect_strategy: Option<ExecServerReconnectStrategy>,
    ) -> Result<Self, ExecServerError> {
        let (rpc_client, events_rx) = RpcClient::new(connection);
        let rpc_client = Arc::new(rpc_client);
        let session_id = OnceLock::new();
        let (connection_changed, _connection_changed_rx) = watch::channel(());
        let inner = Arc::new(Inner {
            instance_id: uuid::Uuid::new_v4().to_string(),
            cleanup_records: StdMutex::new(std::collections::VecDeque::new()),
            cleanup_records_evicted: AtomicU64::new(0),
            unsupported_fs_methods: StdMutex::new(None),
            file_read_slots: Arc::new(tokio::sync::Semaphore::new(
                crate::file_read::MAX_OPEN_FILE_READS,
            )),
            connection: StdMutex::new(ConnectionState {
                status: ConnectionStatus::Connected(Arc::clone(&rpc_client)),
                active_process_starts: 0,
            }),
            connection_changed,
            sessions: ArcSwap::from_pointee(HashMap::new()),
            sessions_write_lock: StdMutex::new(()),
            http_body_streams: Mutex::new(HttpBodyStreams::default()),
            http_body_stream_next_id: AtomicU64::new(1),
            session_id,
            environment_info: OnceLock::new(),
            environment_info_init: Mutex::new(()),
            reconnect_strategy,
        });
        let client = Self {
            inner,
            recovery_policy: RecoveryPolicy::Wait,
        };
        // An explicit resume can redirect notifications from running processes
        // before initialize returns. Drain them immediately so a burst cannot
        // fill the bounded event channel and block the initialize response.
        client.spawn_rpc_reader(&rpc_client, events_rx);
        client.initialize_rpc(&rpc_client, options).await?;
        Ok(client)
    }

    async fn call<P, T>(&self, method: &str, params: &P) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let rpc_client = self.rpc_client().await?;
        self.call_rpc(&rpc_client, method, params).await
    }

    pub(crate) async fn call_rpc<P, T>(
        &self,
        rpc_client: &Arc<RpcClient>,
        method: &str,
        params: &P,
    ) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        map_rpc_call_result(rpc_client.call(method, params).await)
    }

    async fn call_for_cleanup<P, T>(&self, method: &str, params: &P) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        self.call_for_cleanup_before(method, params, Instant::now() + PROCESS_TERMINATION_TIMEOUT)
            .await
    }

    async fn call_for_cleanup_before<P, T>(
        &self,
        method: &str,
        params: &P,
        deadline: Instant,
    ) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let rpc_client = tokio::time::timeout_at(deadline, self.inner.rpc_client())
            .await
            .map_err(|_| {
                ExecServerError::Protocol(format!(
                    "timed out waiting for exec-server `{method}` cleanup after {PROCESS_TERMINATION_TIMEOUT:?}"
                ))
            })??;
        Self::call_for_cleanup_on_before(&rpc_client, method, params, deadline).await
    }

    pub(crate) async fn call_for_cleanup_on_before<P, T>(
        rpc_client: &Arc<RpcClient>,
        method: &str,
        params: &P,
        deadline: Instant,
    ) -> Result<T, ExecServerError>
    where
        P: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let call_timeout = deadline.saturating_duration_since(Instant::now());
        if call_timeout.is_zero() {
            if method == FS_CLOSE_METHOD {
                rpc_client.request_transport_close();
            }
            return Err(ExecServerError::Protocol(format!(
                "timed out waiting for exec-server `{method}` response after {PROCESS_TERMINATION_TIMEOUT:?}"
            )));
        }
        let result = rpc_client
            .call_for_cleanup(method, params, call_timeout)
            .await;
        if method == FS_CLOSE_METHOD && result.is_err() {
            // File handles are connection-local. If their close could not settle,
            // discard this transport so server shutdown releases every handle;
            // keeping it live could strand a close that never entered the queue.
            // Request closure synchronously: waiting for a child supervisor here
            // would extend the already expired cleanup deadline.
            rpc_client.request_transport_close();
        }
        map_rpc_call_result(result)
    }
}

fn map_rpc_call_result<T>(result: Result<T, RpcCallError>) -> Result<T, ExecServerError> {
    result.map_err(|error| {
        let error = ExecServerError::from(error);
        if is_transport_closed_error(&error) {
            ExecServerError::Disconnected(disconnected_message(/*reason*/ None))
        } else {
            error
        }
    })
}

async fn cleanup_process_start(
    client: &ExecServerClient,
    process_id: &ProcessId,
    state: &Arc<SessionState>,
    ticket: &ProcessCleanupTicket,
    submitted: &AtomicBool,
) {
    if !submitted.load(Ordering::Acquire) {
        client.inner.remove_session_if(process_id, state);
        return;
    }
    ticket.update(state.execution_knowledge(), ProcessCleanupStatus::Pending);
    client.inner.retain_cleanup(ticket.clone());
    let mut deadline = Instant::now() + PROCESS_TERMINATION_TIMEOUT;
    let mut poll_delay = Duration::from_millis(10);
    let mut wake = state.subscribe();
    let mut terminal_hint = [false; 3];
    let settlement = loop {
        match client
            .terminate_with_incarnation_before(
                process_id,
                Some(state.incarnation.clone()),
                deadline,
            )
            .await
        {
            Ok(response) if !response.running => break ProcessCleanupStatus::Confirmed,
            Ok(_) => {
                if wait_for_termination_reconciliation(state, &mut wake, &mut terminal_hint, deadline)
                    .await
                    .is_err()
                {
                    ticket.update(
                        state.execution_knowledge(),
                        ProcessCleanupStatus::Unconfirmed(
                            "termination deadline expired; cleanup remains owned".into(),
                        ),
                    );
                    deadline = Instant::now() + PROCESS_TERMINATION_TIMEOUT;
                }
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
            }
            Err(error) if is_transport_closed_error(&error) && !client.inner.is_failed() => {
                tokio::time::sleep(poll_delay).await;
                deadline = Instant::now() + PROCESS_TERMINATION_TIMEOUT;
            }
            Err(error) => {
                ticket.update(
                    state.execution_knowledge(),
                    ProcessCleanupStatus::Unconfirmed(
                        error.to_string().chars().take(4096).collect(),
                    ),
                );
                if client.inner.is_failed() {
                    // Retain the identity/ticket for final enumeration. A failed
                    // observation is not a confirmed process termination.
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                deadline = Instant::now() + PROCESS_TERMINATION_TIMEOUT;
            }
        }
    };
    ticket.update(state.execution_knowledge(), settlement);
    client.inner.remove_session_if(process_id, state);
}

/// Notifications are hints, never termination proof: callers always reconcile
/// through the incarnation-fenced RPC, including its server-side finish step.
/// Ordinary output cannot amplify RPCs, and missing/dropped notifications still
/// reach a bounded fallback inside the existing cleanup deadline.
async fn wait_for_termination_reconciliation(
    state: &SessionState,
    wake: &mut watch::Receiver<u64>,
    terminal_hint: &mut [bool; 3],
    deadline: Instant,
) -> Result<(), tokio::time::error::Elapsed> {
    tokio::time::timeout_at(deadline, async {
        let fallback = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(fallback);
        loop {
            let hint = {
                let events = state.ordered_events.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                [events.observed_exited, events.closed_published, events.failure.is_some()]
            };
            if hint != *terminal_hint && hint.contains(&true) {
                *terminal_hint = hint;
                return;
            }
            tokio::select! {
                biased;
                _ = &mut fallback => return,
                changed = wake.changed() => {
                    if changed.is_err() { return; }
                }
            }
        }
    }).await
}

impl From<RpcCallError> for ExecServerError {
    fn from(value: RpcCallError) -> Self {
        match value {
            RpcCallError::Closed => Self::Closed,
            RpcCallError::Json(err) => Self::Json(err),
            RpcCallError::Server(error) => Self::Server {
                code: error.code,
                message: error.message,
                data: error.data,
            },
            RpcCallError::TimedOut { method, timeout } => Self::Protocol(format!(
                "timed out waiting for exec-server `{method}` response after {timeout:?}"
            )),
            RpcCallError::PendingRequestLimitExceeded { limit } => Self::Protocol(format!(
                "exec-server has reached its limit of {limit} pending requests"
            )),
            RpcCallError::RequestIdExhausted => {
                Self::Protocol("exec-server request IDs are exhausted".to_string())
            }
        }
    }
}

impl SessionState {
    fn execution_knowledge(&self) -> ProcessExecutionKnowledge {
        self.ordered_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_exit_code
            .map_or(
                ProcessExecutionKnowledge::PossiblyExecuted,
                ProcessExecutionKnowledge::ObservedExit,
            )
    }
    fn new(recoverable: bool) -> Self {
        Self::with_event_log(recoverable, ExecProcessEventLog::new(
            PROCESS_EVENT_CHANNEL_CAPACITY,
            PROCESS_EVENT_RETAINED_BYTES,
        ))
    }

    fn with_event_log(recoverable: bool, events: ExecProcessEventLog) -> Self {
        let (wake_tx, _wake_rx) = watch::channel(0);
        Self {
            incarnation: uuid::Uuid::new_v4().to_string(),
            wake_tx,
            events,
            ordered_events: StdMutex::new(OrderedSessionEvents::default()),
            recoverable: AtomicBool::new(recoverable),
            next_write_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    pub(crate) fn subscribe_events(&self) -> ExecProcessEventReceiver {
        self.events.subscribe()
    }

    fn note_change(&self, seq: u64) {
        self.wake_tx
            .send_modify(|current| *current = (*current).max(seq));
    }

    /// Publishes a process event only when all earlier sequenced events have
    /// already been published.
    ///
    /// Returns `true` only when this call actually publishes the ordered
    /// `Closed` event. The caller uses that signal to remove the session route
    /// after the terminal event is visible to subscribers, rather than when a
    /// possibly-early closed notification first arrives.
    fn publish_ordered_event(&self, event: ExecProcessEvent) -> Result<bool, String> {
        let Some(seq) = event.seq() else {
            self.events.publish(event);
            return Ok(false);
        };

        let mut ordered_events = self
            .ordered_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // We have already delivered this sequence number or moved past it,
        // so accepting it again would duplicate output or lifecycle events.
        if ordered_events.failure.is_some()
            || ordered_events.closed_published
            || seq <= ordered_events.last_published_seq
        {
            return Ok(false);
        }

        match &event {
            ExecProcessEvent::Exited { exit_code, sandbox_denied, .. } => {
                ordered_events.observed_exited = true;
                ordered_events.last_exit_code = Some(*exit_code);
                ordered_events.sandbox_denied |= sandbox_denied.unwrap_or(false);
            }
            ExecProcessEvent::Closed { sandbox_denied, .. } => {
                ordered_events.sandbox_denied |= sandbox_denied.unwrap_or(false);
            }
            _ => {}
        }
        ordered_events.insert_pending(event)?;
        Ok(self.publish_ready(&mut ordered_events))
    }

    fn publish_ready(&self, ordered_events: &mut OrderedSessionEvents) -> bool {
        let mut published_closed = false;
        loop {
            let next_seq = ordered_events.last_published_seq.saturating_add(1);
            let Some(event) = ordered_events.pending.remove(&next_seq) else {
                break;
            };
            ordered_events.pending_bytes = ordered_events
                .pending_bytes
                .saturating_sub(pending_process_event_bytes(&event));
            ordered_events.last_published_seq = next_seq;
            ordered_events.exit_published |= matches!(&event, ExecProcessEvent::Exited { .. });
            if let ExecProcessEvent::Exited { exit_code, .. } = &event {
                ordered_events.last_exit_code = Some(*exit_code);
            }
            let is_closed = matches!(&event, ExecProcessEvent::Closed { .. });
            ordered_events.closed_published |= is_closed;
            published_closed |= is_closed;
            self.events.publish(event);
            if is_closed {
                // Closed is terminal, including for later events buffered before
                // the missing prefix arrived or recovery supplied the close.
                ordered_events.pending.clear();
                ordered_events.pending_bytes = 0;
                break;
            }
        }
        published_closed
    }

    fn set_failure(&self, message: String) {
        let mut ordered_events = self
            .ordered_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ordered_events.failure.is_some() || ordered_events.closed_published {
            return;
        }
        ordered_events.failure = Some(message.clone());
        ordered_events.pending.clear();
        ordered_events.pending_bytes = 0;
        self.events.publish(ExecProcessEvent::Failed(message));
        drop(ordered_events);
        self.wake_tx
            .send_modify(|current| *current = current.saturating_add(1));
    }

    fn failed_response(&self) -> Option<ReadResponse> {
        let message = self
            .ordered_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure
            .clone();
        message.map(|message| self.synthesized_failure(message))
    }

    fn observe_read_facts(&self, response: &ReadResponse) {
        let mut events = self.ordered_events.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        events.observed_exited |= response.exited;
        if let Some(exit_code) = response.exit_code { events.last_exit_code = Some(exit_code); }
        events.sandbox_denied |= response.sandbox_denied;
    }

    fn synthesized_failure(&self, message: String) -> ReadResponse {
        let next_seq = (*self.wake_tx.borrow()).saturating_add(1);
        let events = self
            .ordered_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            ;
        ReadResponse {
            chunks: Vec::new(),
            next_seq,
            exited: events.observed_exited || events.exit_published,
            exit_code: events.last_exit_code,
            closed: true,
            failure: Some(message),
            output_gap: None,
            sandbox_denied: events.sandbox_denied,
        }
    }

    fn next_write_id(&self) -> String {
        self.next_write_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string()
    }
}

impl OrderedSessionEvents {
    fn insert_pending(&mut self, event: ExecProcessEvent) -> Result<(), String> {
        let Some(seq) = event.seq() else {
            return Err("cannot reorder an unsequenced process event".to_string());
        };
        if self.pending.contains_key(&seq) {
            return Ok(());
        }

        let next_seq = self.last_published_seq.saturating_add(1);
        // The next expected event is synchronously published by every caller,
        // so it can drain a full buffer without becoming retained state.
        let closes_gap = seq == next_seq;
        if !closes_gap && self.pending.len() >= MAX_PENDING_PROCESS_EVENTS {
            return Err(format!(
                "process event reorder buffer exceeds {MAX_PENDING_PROCESS_EVENTS} entries"
            ));
        }

        let event_bytes = pending_process_event_bytes(&event);
        let byte_limit = self
            .pending_byte_limit
            .unwrap_or(MAX_PENDING_PROCESS_EVENT_BYTES);
        if event_bytes > byte_limit {
            return Err(format!(
                "process event exceeds {MAX_PENDING_PROCESS_EVENT_BYTES} bytes"
            ));
        }
        let pending_bytes = self.pending_bytes.saturating_add(event_bytes);
        if !closes_gap && pending_bytes > byte_limit {
            return Err(format!(
                "process event reorder buffer exceeds {MAX_PENDING_PROCESS_EVENT_BYTES} bytes"
            ));
        }

        self.pending.insert(seq, event);
        self.pending_bytes = pending_bytes;
        Ok(())
    }
}

fn pending_process_event_bytes(event: &ExecProcessEvent) -> usize {
    match event {
        ExecProcessEvent::Output(chunk) => chunk.chunk.0.len(),
        ExecProcessEvent::Failed(message) => message.len(),
        ExecProcessEvent::Exited { .. }
        | ExecProcessEvent::Closed { .. }
        | ExecProcessEvent::OutputGap { .. } => 0,
    }
}

fn finish_process_event(
    inner: &Inner,
    process_id: &ProcessId,
    session: &Arc<SessionState>,
    result: Result<bool, String>,
) {
    match result {
        Ok(true) => inner.remove_session_if(process_id, session),
        Ok(false) => {}
        Err(message) => {
            session.set_failure(message);
            inner.remove_session_if(process_id, session);
        }
    }
}

impl Session {
    pub(crate) fn termination_is_unreachable(&self) -> bool {
        self.client.inner.is_failed()
    }

    pub(crate) fn record_unresolved_termination(&self, reason: &str) {
        let ticket = ProcessCleanupTicket::new(
            self.process_id.clone(),
            self.client.inner.session_id.get().cloned(),
        );
        ticket.update(
            self.state.execution_knowledge(),
            ProcessCleanupStatus::Unconfirmed(reason.chars().take(4096).collect()),
        );
        tracing::warn!(cleanup = %ticket, "remote process termination remains unconfirmed");
        self.client.inner.retain_cleanup(ticket);
    }
    pub(crate) fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    pub(crate) fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.state.subscribe()
    }

    pub(crate) fn subscribe_events(&self) -> ExecProcessEventReceiver {
        self.state.subscribe_events()
    }

    pub(crate) async fn read(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> Result<ReadResponse, ExecServerError> {
        loop {
            if let Some(response) = self.state.failed_response() {
                return Ok(response);
            }

            match self
                .client
                .call::<_, ReadResponse>(
                    EXEC_READ_METHOD,
                    &crate::protocol::OwnedProcessParams {
                        incarnation: Some(self.state.incarnation.clone()),
                        params: ReadParams {
                            process_id: self.process_id.clone(),
                            after_seq,
                            max_bytes,
                            wait_ms,
                        },
                    },
                )
                .await
            {
                Ok(response) => {
                    self.state.observe_read_facts(&response);
                    if let Some(message) = &response.failure {
                        self.state.set_failure(message.clone());
                    }
                    return Ok(response);
                }
                Err(error)
                    if is_transport_closed_error(&error)
                        && matches!(self.client.recovery_policy, RecoveryPolicy::Wait)
                        && !self.client.inner.is_failed() =>
                {
                    continue;
                }
                Err(error) if is_transport_closed_error(&error) && self.client.inner.is_failed() => {
                    if let Some(response) = self.state.failed_response() {
                        return Ok(response);
                    }
                    let message = error.to_string();
                    self.state.set_failure(message.clone());
                    return Ok(self.state.synthesized_failure(message));
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) async fn write(&self, chunk: Vec<u8>) -> Result<WriteResponse, ExecServerError> {
        let params = crate::protocol::OwnedProcessParams {
            incarnation: Some(self.state.incarnation.clone()),
            params: WriteParams {
                process_id: self.process_id.clone(),
                chunk: chunk.into(),
                write_id: self.state.next_write_id(),
            },
        };
        loop {
            match self.client.call(EXEC_WRITE_METHOD, &params).await {
                Ok(response) => return Ok(response),
                Err(error)
                    if is_transport_closed_error(&error)
                        && matches!(self.client.recovery_policy, RecoveryPolicy::Wait)
                        && !self.client.inner.is_failed() =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) async fn signal(&self, signal: ProcessSignal) -> Result<(), ExecServerError> {
        let _: SignalResponse = self
            .client
            .call(
                EXEC_SIGNAL_METHOD,
                &crate::protocol::OwnedProcessParams {
                    incarnation: Some(self.state.incarnation.clone()),
                    params: SignalParams {
                        process_id: self.process_id.clone(),
                        signal,
                    },
                },
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn terminate(&self) -> Result<(), ExecServerError> {
        let deadline = Instant::now() + PROCESS_TERMINATION_TIMEOUT;
        let mut wake = self.state.subscribe();
        let mut terminal_hint = [false; 3];
        loop {
            match self
                .client
                .terminate_with_incarnation_before(
                    &self.process_id,
                    Some(self.state.incarnation.clone()),
                    deadline,
                )
                .await
            {
                Ok(response) if !response.running => return Ok(()),
                Ok(_) => {
                    wait_for_termination_reconciliation(
                        &self.state,
                        &mut wake,
                        &mut terminal_hint,
                        deadline,
                    )
                    .await
                    .map_err(|_| {
                        ExecServerError::Protocol(format!(
                            "timed out confirming process termination after {PROCESS_TERMINATION_TIMEOUT:?}"
                        ))
                    })?;
                }
                Err(error)
                    if is_transport_closed_error(&error) && !self.client.inner.is_failed() =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn unregister(&self) {
        // Failed replay retains this identity until its cleanup owner finishes.
        if !self.state.recoverable.load(Ordering::Acquire) {
            return;
        }
        self.client
            .inner
            .remove_session_if(&self.process_id, &self.state);
    }
}

impl Inner {
    fn retain_cleanup(&self, ticket: ProcessCleanupTicket) {
        let mut records = self
            .cleanup_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if records
            .iter()
            .any(|existing| Arc::ptr_eq(&existing.0, &ticket.0))
        {
            return;
        }
        if records.len() == 128 {
            records.pop_front();
            self.cleanup_records_evicted.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("old process cleanup evidence evicted from bounded history");
        }
        records.push_back(ticket);
    }
    fn get_session(&self, process_id: &str) -> Option<Arc<SessionState>> {
        self.sessions.load().get(process_id).cloned()
    }

    fn insert_session(
        &self,
        process_id: &ProcessId,
        session: Arc<SessionState>,
    ) -> Result<(), ExecServerError> {
        let _sessions_write_guard = self
            .sessions_write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Do not register a process session that can never receive environment
        // notifications. Without this check, remote MCP startup could create a
        // dead session and wait for process output that will never arrive.
        if let Some(message) = self.failure_message() {
            return Err(ExecServerError::Disconnected(message));
        }
        let sessions = self.sessions.load();
        if sessions.contains_key(process_id) {
            return Err(ExecServerError::Protocol(format!(
                "session already registered for process {process_id}"
            )));
        }
        let mut next_sessions = sessions.as_ref().clone();
        next_sessions.insert(process_id.clone(), session);
        self.sessions.store(Arc::new(next_sessions));
        Ok(())
    }

    fn remove_session_if(&self, process_id: &ProcessId, expected: &Arc<SessionState>) {
        let _sessions_write_guard = self
            .sessions_write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sessions = self.sessions.load();
        if !sessions
            .get(process_id)
            .is_some_and(|session| Arc::ptr_eq(session, expected))
        {
            return;
        }
        let mut next_sessions = sessions.as_ref().clone();
        next_sessions.remove(process_id);
        self.sessions.store(Arc::new(next_sessions));
    }

    fn take_all_sessions(&self) -> HashMap<ProcessId, Arc<SessionState>> {
        let _sessions_write_guard = self
            .sessions_write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sessions = self.sessions.load();
        let drained_sessions = sessions.as_ref().clone();
        self.sessions.store(Arc::new(HashMap::new()));
        drained_sessions
    }
}

fn disconnected_message(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("exec-server transport disconnected: {reason}"),
        None => "exec-server transport disconnected".to_string(),
    }
}

fn is_transport_closed_error(error: &ExecServerError) -> bool {
    matches!(
        error,
        ExecServerError::Closed | ExecServerError::Disconnected(_)
    ) || matches!(
        error,
        ExecServerError::Server {
            code: -32000,
            message,
            ..
        } if message == "JSON-RPC transport closed"
    )
}

fn fail_all_sessions(inner: &Arc<Inner>, message: String) {
    let sessions = inner.take_all_sessions();

    for (_, session) in sessions {
        // Sessions synthesize a closed read response and emit a pushed Failed
        // event. That covers both polling consumers and streaming consumers
        // such as environment-backed MCP stdio.
        session.set_failure(message.clone());
    }
}

/// Fails all in-flight work that depends on the shared JSON-RPC transport.
async fn fail_all_in_flight_work(inner: &Arc<Inner>, message: String) {
    fail_all_sessions(inner, message.clone());
    inner.fail_all_http_body_streams(message).await;
}

async fn handle_server_notification(
    inner: &Arc<Inner>,
    notification: JSONRPCNotification,
) -> Result<(), ExecServerError> {
    match notification.method.as_str() {
        EXEC_FAILED_METHOD => {
            let params: ExecFailedNotification =
                serde_json::from_value(notification.params.unwrap_or(Value::Null))?;
            if let Some(session) = inner.get_session(&params.process_id) {
                session.set_failure(params.message);
            }
        }
        EXEC_OUTPUT_DELTA_METHOD => {
            let value = notification.params.unwrap_or(Value::Null);
            let unregistered = value.get("processId").and_then(Value::as_str)
                .is_some_and(|id| inner.get_session(id).is_none());
            let (mut params, deferred): (ExecOutputDeltaNotification, _) = if unregistered {
                deferred_notification::parse_without_chunk(value, "chunk")?
            } else {
                (serde_json::from_value(value)?, None)
            };
            if let Some(encoded) = deferred {
                // Registration may race validation. Never deliver to a route
                // captured before expensive decoding, and never skip malformed bytes.
                if inner.get_session(&params.process_id).is_none() { return Ok(()); }
                params.chunk = serde_json::from_value(Value::String(encoded))?;
            }
            if let Some(session) = inner.get_session(&params.process_id) {
                let result =
                    session.publish_ordered_event(ExecProcessEvent::Output(ProcessOutputChunk {
                        seq: params.seq,
                        stream: params.stream,
                        chunk: params.chunk,
                    }));
                if result.is_ok() {
                    session.note_change(params.seq);
                }
                finish_process_event(inner, &params.process_id, &session, result);
            }
        }
        EXEC_EXITED_METHOD => {
            let params: ExecExitedNotification =
                serde_json::from_value(notification.params.unwrap_or(Value::Null))?;
            if let Some(session) = inner.get_session(&params.process_id) {
                let result = session.publish_ordered_event(ExecProcessEvent::Exited {
                    seq: params.seq,
                    exit_code: params.exit_code,
                    sandbox_denied: params.sandbox_denied,
                });
                if result.is_ok() {
                    session.note_change(params.seq);
                }
                finish_process_event(inner, &params.process_id, &session, result);
            }
        }
        EXEC_CLOSED_METHOD => {
            let params: ExecClosedNotification =
                serde_json::from_value(notification.params.unwrap_or(Value::Null))?;
            if let Some(session) = inner.get_session(&params.process_id) {
                // Closed is terminal, but it can arrive before tail output or
                // exited. Keep routing this process until the ordered publisher
                // says Closed has actually been delivered.
                let result = session.publish_ordered_event(ExecProcessEvent::Closed {
                    seq: params.seq,
                    sandbox_denied: params.sandbox_denied,
                });
                if result.is_ok() {
                    session.note_change(params.seq);
                }
                finish_process_event(inner, &params.process_id, &session, result);
            }
        }
        crate::protocol::EXEC_OUTPUT_GAP_METHOD => {
            let params: crate::protocol::ExecOutputGapNotification =
                serde_json::from_value(notification.params.unwrap_or(Value::Null))?;
            if let Some(session) = inner.get_session(&params.process_id) {
                // Capture loss occupies its sequence without bytes; publishing
                // it in order releases events already queued behind it.
                let result = session.publish_ordered_event(ExecProcessEvent::OutputGap {
                    through_seq: params.through_seq,
                });
                if result.is_ok() {
                    session.note_change(params.through_seq);
                }
                finish_process_event(inner, &params.process_id, &session, result);
            }
        }
        HTTP_REQUEST_BODY_DELTA_METHOD => {
            inner
                .handle_http_body_delta_notification(notification.params)
                .await?;
        }
        other => {
            debug!("ignoring unknown exec-server notification: {other}");
        }
    }
    Ok(())
}


