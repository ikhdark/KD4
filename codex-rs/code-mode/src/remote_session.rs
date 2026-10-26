use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSession;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::CodeModeSessionProvider;
use codex_code_mode_protocol::CodeModeSessionProviderFuture;
use codex_code_mode_protocol::CodeModeSessionResultFuture;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::RUNTIME_REPLACED_NOTICE;
use codex_code_mode_protocol::StartedCell;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::SessionId;
use futures::FutureExt;
use tokio::sync::Semaphore;
use tokio::sync::watch;

use self::connection::Connection;
use self::connection::RemoteSession;
use self::connection::SessionCleanup;
use crate::NoopCodeModeSessionDelegate;

mod connection;

const CODE_MODE_HOST_PATH_ENV: &str = "CODEX_CODE_MODE_HOST_PATH";

type ShutdownResultReceiver = watch::Receiver<Option<Result<(), String>>>;
type PendingWait = Arc<tokio::sync::Mutex<Option<RetainedWait>>>;

/// A cell wait the session keeps receiving after its observer drops.
struct RetainedWait {
    future: CodeModeSessionResultFuture<'static, WaitOutcome>,
    /// Asks the host to release this observation while it is still received.
    release: tokio_util::sync::CancellationToken,
}

/// Creates code-mode sessions backed by one lazily spawned process host.
pub struct ProcessOwnedCodeModeSessionProvider {
    host_program: PathBuf,
    process_host: StdMutex<Option<Arc<OwnedProcessHost>>>,
}

impl ProcessOwnedCodeModeSessionProvider {
    pub fn with_host_program(host_program: PathBuf) -> Self {
        Self {
            host_program,
            process_host: StdMutex::new(None),
        }
    }

    fn process_host(&self) -> Arc<OwnedProcessHost> {
        let mut process_host = self
            .process_host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(process_host) = process_host.as_ref() {
            return Arc::clone(process_host);
        }

        let new_process_host = Arc::new(OwnedProcessHost::new(self.host_program.clone()));
        *process_host = Some(Arc::clone(&new_process_host));
        new_process_host
    }
}

impl Default for ProcessOwnedCodeModeSessionProvider {
    fn default() -> Self {
        Self::with_host_program(default_host_program())
    }
}

impl CodeModeSessionProvider for ProcessOwnedCodeModeSessionProvider {
    fn availability(&self) -> Result<(), String> {
        if self.host_program.is_file() {
            Ok(())
        } else {
            Err(format!(
                "failed to spawn code-mode host {}: host executable was not found",
                self.host_program.display()
            ))
        }
    }

    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        let session = ProcessOwnedCodeModeSession::with_process_host(delegate, self.process_host());
        Box::pin(async move {
            session.connection().await?;
            let session: Arc<dyn CodeModeSession> = Arc::new(session);
            Ok(session)
        })
    }
}

/// Used when `features.code_mode_host` is off: Codex never runs cells in its
/// own process, so no session can be created.
pub struct DisabledCodeModeSessionProvider;

impl CodeModeSessionProvider for DisabledCodeModeSessionProvider {
    fn availability(&self) -> Result<(), String> {
        Err("code-mode host is disabled".to_string())
    }

    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async { Err("code-mode host is disabled".to_string()) })
    }
}

struct OwnedProcessHost {
    host_program: PathBuf,
    state: StdMutex<OwnedProcessHostState>,
    spawn_permit: Semaphore,
    next_session_id: AtomicU64,
}

#[derive(Default)]
struct OwnedProcessHostState {
    connection: Option<Arc<Connection>>,
    active_sessions: usize,
    /// Counts failed spawns so callers queued behind one share its error.
    failed_spawns: u64,
    last_spawn_error: Option<String>,
}

struct HostSessionLease {
    process_host: Arc<OwnedProcessHost>,
}

impl OwnedProcessHost {
    fn new(host_program: PathBuf) -> Self {
        Self {
            host_program,
            state: StdMutex::new(OwnedProcessHostState::default()),
            spawn_permit: Semaphore::new(/*permits*/ 1),
            next_session_id: AtomicU64::new(1),
        }
    }

    async fn connection(&self) -> Result<Arc<Connection>, String> {
        if let Some(connection) = self.live_connection() {
            connection.observe_reference();
            return Ok(connection);
        }
        let observed_failures = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failed_spawns;

        let _spawn_permit = self
            .spawn_permit
            .acquire()
            .await
            .map_err(|_| "code-mode host spawn coordinator closed".to_string())?;
        if let Some(connection) = self.live_connection() {
            connection.observe_reference();
            return Ok(connection);
        }
        {
            // A spawn that failed while this caller waited is its answer too;
            // retrying would make each queued caller wait out another
            // handshake timeout. Later callers still retry.
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.failed_spawns != observed_failures
                && let Some(error) = state.last_spawn_error.as_ref()
            {
                return Err(error.clone());
            }
        }
        let spawned = Connection::spawn(&self.host_program).await;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match spawned {
            Ok(connection) => {
                let connection = Arc::new(connection);
                state.connection = Some(Arc::clone(&connection));
                state.last_spawn_error = None;
                Ok(connection)
            }
            Err(error) => {
                state.failed_spawns += 1;
                state.last_spawn_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn live_connection(&self) -> Option<Arc<Connection>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connection
            .as_ref()
            .filter(|connection| connection.is_alive())
            .cloned()
    }

    fn acquire_session(self: &Arc<Self>) -> HostSessionLease {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_sessions += 1;
        HostSessionLease {
            process_host: Arc::clone(self),
        }
    }

    fn allocate_session_id(&self) -> SessionId {
        let value = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        match SessionId::new(format!("session-{value}")) {
            Ok(session_id) => session_id,
            Err(_) => unreachable!("a generated code-mode session ID is nonempty"),
        }
    }
}

impl Drop for HostSessionLease {
    fn drop(&mut self) {
        let connection = {
            let mut state = self
                .process_host
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            debug_assert!(
                state.active_sessions > 0,
                "code-mode host session lease count underflow"
            );
            state.active_sessions = state.active_sessions.saturating_sub(1);
            if state.active_sessions == 0 {
                state.connection.take()
            } else {
                None
            }
        };
        drop(connection);
    }
}

enum SessionState {
    New,
    Opening {
        remote: RemoteSession,
        result_rx: watch::Receiver<Option<Result<SessionBinding, String>>>,
    },
    Open(SessionBinding),
    Closing,
    Closed,
}

#[derive(Clone)]
struct SessionBinding {
    connection: Arc<Connection>,
    remote: RemoteSession,
    cleanup: SessionCleanup,
}

struct SessionInner {
    runtime: OnceLock<tokio::runtime::Handle>,
    process_host: Arc<OwnedProcessHost>,
    host_lease: StdMutex<Option<HostSessionLease>>,
    delegate: Arc<dyn CodeModeSessionDelegate>,
    state: StdMutex<SessionState>,
    next_generation: AtomicU64,
    /// The generation that last started a cell; 0 before any cell.
    executed_generation: AtomicU64,
    /// That generation's host exited, taking its stored values; the next
    /// `execute` reports this instead of running.
    lost_state: AtomicBool,
    shutdown_requested: AtomicBool,
    shutdown_result: StdMutex<Option<ShutdownResultReceiver>>,
    // Retiring a binding must not drop the last connection owner while its
    // supervisor still drains accepted stdout responses.
    retired_cleanups: StdMutex<Vec<(SessionCleanup, Arc<Connection>)>>,
    // Keep the receiving future, not just a successful intermediate send. A
    // steered caller can drop its observation without discarding a host yield.
    pending_waits: StdMutex<std::collections::HashMap<(u64, CellId), PendingWait>>,
}

/// A logical code-mode session assigned to a process-owned host.
pub struct ProcessOwnedCodeModeSession {
    inner: Arc<SessionInner>,
}

impl ProcessOwnedCodeModeSession {
    pub fn new() -> Self {
        Self::with_process_host(
            Arc::new(NoopCodeModeSessionDelegate),
            Arc::new(OwnedProcessHost::new(default_host_program())),
        )
    }

    fn with_process_host(
        delegate: Arc<dyn CodeModeSessionDelegate>,
        process_host: Arc<OwnedProcessHost>,
    ) -> Self {
        let host_lease = process_host.acquire_session();
        Self {
            inner: Arc::new(SessionInner {
                runtime: OnceLock::new(),
                process_host,
                host_lease: StdMutex::new(Some(host_lease)),
                delegate,
                state: StdMutex::new(SessionState::New),
                next_generation: AtomicU64::new(1),
                executed_generation: AtomicU64::new(0),
                lost_state: AtomicBool::new(false),
                shutdown_requested: AtomicBool::new(false),
                shutdown_result: StdMutex::new(None),
                retired_cleanups: StdMutex::new(Vec::new()),
                pending_waits: StdMutex::new(std::collections::HashMap::new()),
            }),
        }
    }

    async fn connection(&self) -> Result<SessionBinding, String> {
        self.inner.connection().await
    }

    pub async fn execute(&self, request: ExecuteRequest) -> Result<StartedCell, String> {
        let binding = self.connection().await?;
        if self.inner.lost_state.swap(false, Ordering::AcqRel) {
            return Err(RUNTIME_REPLACED_NOTICE.to_string());
        }
        self.inner.record_execution_generation(binding.remote.generation);
        binding.connection.execute(binding.remote, request).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The per-cell guard owns the in-flight receiving future so a dropped observer leaves it in session custody; only waits for the same cell contend"
    )]
    pub async fn wait(&self, request: WaitRequest) -> Result<WaitOutcome, String> {
        let binding = self.connection().await?;
        let key = (binding.remote.generation, request.cell_id.clone());
        let pending = {
            let mut waits = self
                .inner
                .pending_waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.inner.shutdown_requested.load(Ordering::Acquire) {
                return Err("code mode session is shutting down".into());
            }
            prune_pending_waits(&mut waits, key.0);
            if !waits.contains_key(&key)
                && waits.len() >= codex_code_mode_protocol::host::MAX_IN_FLIGHT_REQUESTS
            {
                return Err(
                    "code-mode pending observation capacity reached; resume an existing cell"
                        .into(),
                );
            }
            Arc::clone(
                waits
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
            )
        };
        // Only this cell is serialized. Other cells and shutdown never wait on
        // this mutex; dropping the observer leaves the future in session custody.
        let mut receiving = pending.lock().await;
        let mut retained_result = None;
        if let Some(retained) = receiving.as_mut() {
            // A host answer that reached the dropped observer belongs to this
            // caller. A still-held observation must not hold this wait to its
            // older yield deadline: release it, but keep receiving so a yield
            // already sent by the host is delivered rather than discarded.
            let result = match retained.future.as_mut().now_or_never() {
                Some(result) => result,
                None => {
                    retained.release.cancel();
                    retained.future.as_mut().await
                }
            };
            let released = retained.release.is_cancelled();
            *receiving = None;
            if result.is_ok() || !released {
                retained_result = Some(result);
            }
        }
        let result = match retained_result {
            Some(result) => result,
            None => {
                let release = tokio_util::sync::CancellationToken::new();
                let host_release = release.clone();
                let retained = receiving.insert(RetainedWait {
                    future: Box::pin(async move {
                        binding
                            .connection
                            .wait_releasable(binding.remote, request, host_release)
                            .await
                    }),
                    release,
                });
                let result = retained.future.as_mut().await;
                *receiving = None;
                result
            }
        };
        let mut waits = self
            .inner
            .pending_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::strong_count(&pending) == 2
            && waits
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &pending))
        {
            waits.remove(&key);
        }
        result
    }

    pub async fn terminate(&self, cell_id: CellId) -> Result<WaitOutcome, String> {
        let binding = self.connection().await?;
        let key = (binding.remote.generation, cell_id.clone());
        let result = binding.connection.terminate(binding.remote, cell_id).await;
        if result.is_ok() {
            self.inner
                .pending_waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
        }
        result
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            wait_for_watch(self.inner.request_shutdown()),
        )
        .await
        .map_err(|_| codex_code_mode_protocol::host::SESSION_SHUTDOWN_PENDING.to_string())?
    }
}

fn prune_pending_waits(
    waits: &mut std::collections::HashMap<(u64, CellId), PendingWait>,
    generation: u64,
) {
    waits.retain(|(entry_generation, _), pending| {
        // An old caller is not authority to retire a replacement host's waits.
        if *entry_generation < generation {
            return false;
        }
        // A queued observer may disappear after the completing observer left an
        // empty slot. Never block on a cell mutex while holding the map lock.
        !(Arc::strong_count(pending) == 1
            && pending.try_lock().is_ok_and(|receiving| receiving.is_none()))
    });
}

impl SessionInner {
    fn record_execution_generation(&self, generation: u64) {
        // A caller holding an obsolete binding cannot erase a newer host's state.
        self.executed_generation.fetch_max(generation, Ordering::AcqRel);
    }

    async fn connection(self: &Arc<Self>) -> Result<SessionBinding, String> {
        self.runtime.get_or_init(tokio::runtime::Handle::current);
        loop {
            if self.shutdown_requested.load(Ordering::Acquire) {
                return Err("code mode session is shutting down".to_string());
            }
            let (result_rx, start) = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &*state {
                    SessionState::New => {
                        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                        let remote = RemoteSession {
                            id: self.process_host.allocate_session_id(),
                            generation,
                        };
                        let (result_tx, result_rx) = watch::channel(None);
                        *state = SessionState::Opening {
                            remote: remote.clone(),
                            result_rx: result_rx.clone(),
                        };
                        (result_rx, Some((remote, result_tx)))
                    }
                    SessionState::Opening { result_rx, .. } => (result_rx.clone(), None),
                    SessionState::Open(binding) if binding.connection.is_alive() => {
                        return Ok(binding.clone());
                    }
                    SessionState::Open(binding) => {
                        if self.executed_generation.load(Ordering::Acquire)
                            == binding.remote.generation
                        {
                            self.lost_state.store(true, Ordering::Release);
                        }
                        self.retain_cleanup(binding);
                        *state = SessionState::New;
                        continue;
                    }
                    SessionState::Closing | SessionState::Closed => {
                        return Err("code mode session is shutting down".to_string());
                    }
                }
            };
            if let Some((remote, result_tx)) = start {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    inner.open(remote, result_tx).await;
                });
            }
            return wait_for_watch(result_rx).await;
        }
    }

    async fn open(
        self: Arc<Self>,
        remote: RemoteSession,
        result_tx: watch::Sender<Option<Result<SessionBinding, String>>>,
    ) {
        let result = match self.process_host.connection().await {
            Ok(connection) => {
                let cleanup = connection
                    .open_session(remote.clone(), Arc::clone(&self.delegate))
                    .await;
                cleanup.map(|cleanup| SessionBinding {
                    connection,
                    remote: remote.clone(),
                    cleanup,
                })
            }
            Err(err) => Err(err),
        };
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(
                &*state,
                SessionState::Opening {
                    remote: opening_remote,
                    ..
                } if opening_remote == &remote
            ) {
                *state = match &result {
                    Ok(binding) => SessionState::Open(binding.clone()),
                    Err(_) => SessionState::New,
                };
            }
        }
        result_tx.send_replace(Some(result));
    }

    fn request_shutdown(self: &Arc<Self>) -> ShutdownResultReceiver {
        self.shutdown_requested.store(true, Ordering::Release);
        self.pending_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let mut shutdown_result = self
            .shutdown_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(result_rx) = shutdown_result.as_ref() {
            return result_rx.clone();
        }
        let (result_tx, result_rx) = watch::channel(None);
        *shutdown_result = Some(result_rx.clone());
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let result = inner.drive_shutdown().await;
            result_tx.send_replace(Some(result));
        });
        result_rx
    }

    async fn drive_shutdown(self: &Arc<Self>) -> Result<(), String> {
        loop {
            let action = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &*state {
                    SessionState::New => {
                        *state = SessionState::Closing;
                        ShutdownAction::Finish
                    }
                    SessionState::Opening { result_rx, .. } => {
                        ShutdownAction::WaitForOpen(result_rx.clone())
                    }
                    SessionState::Open(binding) if !binding.connection.is_alive() => {
                        let cleanup = binding.cleanup.clone();
                        *state = SessionState::Closing;
                        ShutdownAction::WaitForSessionCleanup(cleanup)
                    }
                    SessionState::Open(binding) => {
                        let binding = binding.clone();
                        *state = SessionState::Closing;
                        ShutdownAction::Close(binding)
                    }
                    SessionState::Closing => {
                        return Err("code-mode session shutdown driver entered twice".to_string());
                    }
                    SessionState::Closed => return Ok(()),
                }
            };
            match action {
                ShutdownAction::WaitForOpen(result_rx) => {
                    let _ = wait_for_watch(result_rx).await;
                }
                ShutdownAction::Finish => {
                    self.wait_for_retired_cleanups().await;
                    self.transition_to_closed();
                    return Ok(());
                }
                ShutdownAction::WaitForSessionCleanup(cleanup) => {
                    cleanup.wait().await;
                    self.wait_for_retired_cleanups().await;
                    self.transition_to_closed();
                    return Ok(());
                }
                ShutdownAction::Close(binding) => {
                    let jitter = std::collections::hash_map::RandomState::new();
                    let mut retry = 0u32;
                    let result = loop {
                        let result = binding
                            .connection
                            .shutdown_session(binding.remote.clone())
                            .await;
                        if result.as_ref().is_err_and(|error| {
                            error == codex_code_mode_protocol::host::SESSION_SHUTDOWN_PENDING
                        }) {
                            // Only re-observe close, never execution. The owned
                            // driver and host lease survive an observer timeout.
                            let ceiling = 100u64.saturating_mul(1u64 << retry.min(5)).min(2_000);
                            let delay = ceiling / 2 + std::hash::BuildHasher::hash_one(&jitter, retry)
                                % (ceiling / 2 + 1);
                            retry = retry.saturating_add(1);
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                            continue;
                        }
                        break result;
                    };
                    if result.is_err() && !binding.connection.is_alive() {
                        binding.cleanup.wait().await;
                    }
                    self.wait_for_retired_cleanups().await;
                    self.transition_to_closed();
                    return result;
                }
            }
        }
    }

    fn transition_to_closed(&self) {
        let lease = self
            .host_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(lease);
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SessionState::Closed;
    }

    fn retain_cleanup(&self, binding: &SessionBinding) {
        let mut retired = self
            .retired_cleanups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retired.retain(|(cleanup, _)| !cleanup.is_complete());
        if !binding.cleanup.is_complete() {
            retired.push((binding.cleanup.clone(), Arc::clone(&binding.connection)));
        }
    }

    async fn wait_for_retired_cleanups(&self) {
        let retired = std::mem::take(
            &mut *self
                .retired_cleanups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (cleanup, connection) in retired {
            cleanup.wait().await;
            drop(connection);
        }
    }
}

enum ShutdownAction {
    WaitForOpen(watch::Receiver<Option<Result<SessionBinding, String>>>),
    Finish,
    WaitForSessionCleanup(SessionCleanup),
    Close(SessionBinding),
}

async fn wait_for_watch<T>(
    mut result_rx: watch::Receiver<Option<Result<T, String>>>,
) -> Result<T, String>
where
    T: Clone,
{
    loop {
        if let Some(result) = result_rx.borrow().clone() {
            return result;
        }
        result_rx
            .changed()
            .await
            .map_err(|_| "code-mode session transition stopped".to_string())?;
    }
}

impl Drop for ProcessOwnedCodeModeSession {
    fn drop(&mut self) {
        // A Send session may be dropped on a thread that has not entered Tokio.
        // Keep cleanup on the runtime that owns its connection and host tasks.
        if let Some(runtime) = self
            .inner
            .runtime
            .get()
            .cloned()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        {
            let _entered = runtime.enter();
            self.inner.request_shutdown();
        }
    }
}

impl Default for ProcessOwnedCodeModeSession {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeModeSession for ProcessOwnedCodeModeSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(ProcessOwnedCodeModeSession::execute(self, request))
    }

    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(ProcessOwnedCodeModeSession::wait(self, request))
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(ProcessOwnedCodeModeSession::terminate(self, cell_id))
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(ProcessOwnedCodeModeSession::shutdown(self))
    }
}

fn default_host_program() -> PathBuf {
    resolve_host_program(
        std::env::var_os(CODE_MODE_HOST_PATH_ENV),
        std::env::current_exe(),
    )
}

fn resolve_host_program(
    override_path: Option<OsString>,
    current_exe: io::Result<PathBuf>,
) -> PathBuf {
    if let Some(path) = override_path {
        return PathBuf::from(path);
    }
    let executable_name = "codex-code-mode-host.exe";
    if let Ok(current_exe) = current_exe
        && let Some(parent) = current_exe.parent()
    {
        return parent.join(executable_name);
    }
    PathBuf::from(executable_name)
}
