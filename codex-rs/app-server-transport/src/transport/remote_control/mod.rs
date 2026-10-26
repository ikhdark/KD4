mod auth;
mod client_tracker;
mod clients;
mod desired_state;
mod enroll;
mod protocol;
mod segment;
mod server_api;
mod websocket;

use self::auth::load_remote_control_auth;
use self::auth::recover_remote_control_auth;
use self::desired_state::RemoteControlDesiredState;
use self::desired_state::acquire_persistence_lock;
use self::enroll::RemoteControlEnrollment;
use self::enroll::load_persisted_remote_control_enrollment;
use self::enroll::update_persisted_remote_control_enrollment;
use self::server_api::enroll_remote_control_server_with_client;
use self::server_api::refresh_remote_control_server_with_client;
use crate::transport::remote_control::websocket::RemoteControlChannels;
use crate::transport::remote_control::websocket::RemoteControlStatusPublisher;
use crate::transport::remote_control::websocket::RemoteControlWebsocket;

pub use self::protocol::ClientId;
use self::protocol::RemoteControlPairingStatusCode;
use self::protocol::ServerEvent;
use self::protocol::StreamId;
use self::protocol::normalize_remote_control_url;
use super::CHANNEL_CAPACITY;
use super::TransportEvent;
use super::next_connection_id;
pub(super) use codex_api::X_CODEX_INSTALLATION_ID_HEADER as REMOTE_CONTROL_INSTALLATION_ID_HEADER;
use codex_app_server_protocol::RemoteControlClientsListParams;
use codex_app_server_protocol::RemoteControlClientsListResponse;
use codex_app_server_protocol::RemoteControlClientsRevokeParams;
use codex_app_server_protocol::RemoteControlClientsRevokeResponse;
use codex_app_server_protocol::RemoteControlConnectionStatus;
use codex_app_server_protocol::RemoteControlPairingStartParams;
use codex_app_server_protocol::RemoteControlPairingStartResponse;
use codex_app_server_protocol::RemoteControlPairingStatusParams;
use codex_app_server_protocol::RemoteControlPairingStatusResponse;
use codex_app_server_protocol::RemoteControlStatusChangedNotification;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthManager;
use codex_login::auth::PolicyIdentity;
use codex_login::default_client::create_client_pool_without_request_logging;
use codex_state::StateRuntime;
use futures::FutureExt;
use gethostname::gethostname;
use std::error::Error;
use std::fmt;
use std::io;
use std::ops::Deref;
use std::ops::DerefMut;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;

fn remote_control_http_clients(factory: HttpClientFactory) -> RouteAwareClientPool {
    create_client_pool_without_request_logging(factory, ClientRouteClass::Api)
}

pub struct RemoteControlStartConfig {
    pub remote_control_url: String,
    pub installation_id: String,
    pub policy: RemoteControlPolicy,
    pub http_client_factory: HttpClientFactory,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RemoteControlPolicy {
    #[default]
    Allowed,
    DisabledByRequirements,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteControlStartupMode {
    ResolvePersisted,
    DisabledEphemeral,
    EnabledEphemeral,
}

/// Internal marker used by the daemon to disable remote control without requiring a new CLI flag.
pub const REMOTE_CONTROL_DISABLED_ENV_VAR: &str =
    "CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED";

/// Reads and removes the daemon's internal disabled-start marker before worker threads start.
pub fn take_remote_control_disabled_env() -> bool {
    let disabled =
        std::env::var_os(REMOTE_CONTROL_DISABLED_ENV_VAR).is_some_and(|value| value == "1");
    // SAFETY: app-server calls this synchronously at process startup, before spawning threads.
    unsafe { std::env::remove_var(REMOTE_CONTROL_DISABLED_ENV_VAR) };
    disabled
}

pub(super) struct QueuedServerEnvelope {
    pub(super) event: ServerEvent,
    pub(super) client_id: ClientId,
    pub(super) stream_id: StreamId,
    pub(super) write_complete_tx: Option<oneshot::Sender<std::time::Instant>>,
}

#[derive(Clone)]
pub struct RemoteControlHandle {
    policy: Arc<watch::Sender<RemoteControlPolicyDecision>>,
    desired_state_tx: Arc<watch::Sender<RemoteControlDesiredState>>,
    desired_state_rpc_lock: Arc<Semaphore>,
    desired_state_persistence_lock: Arc<Semaphore>,
    status_tx: Arc<watch::Sender<RemoteControlStatusChangedNotification>>,
    state_db: Option<Arc<StateRuntime>>,
    remote_control_url: String,
    current_enrollment: CurrentRemoteControlEnrollment,
    pairing_persistence_key: RemoteControlPairingPersistenceKey,
    pairing_persistence_key_required: bool,
    auth_manager: Arc<AuthManager>,
    http_clients: RouteAwareClientPool,
}

#[derive(Clone, Copy)]
enum RemoteControlHttpClientSource<'a> {
    Pool(&'a RouteAwareClientPool, &'a RemoteControlEnrollmentState),
}

impl<'a> RemoteControlHttpClientSource<'a> {
    fn request_state(self) -> &'a RemoteControlEnrollmentState {
        match self {
            Self::Pool(_, state) => state,
        }
    }

    async fn client_for_url(self, url: &str) -> io::Result<HttpClient> {
        match self {
            Self::Pool(http_clients, _) => http_clients
                .client_for_url(url)
                .await
                .map_err(io::Error::other),
        }
    }
}

// Pairing and websocket connect share one selected server so they cannot enroll or replace
// different persisted rows while either path is awaiting backend I/O.
type CurrentRemoteControlEnrollment = Arc<RemoteControlEnrollmentState>;
type RemoteControlPairingPersistenceKey = watch::Sender<Option<String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteControlEnrollmentSelection {
    ReuseOrCreate,
    ReplaceExisting,
}

struct RemoteControlEnrollmentState {
    enrollment: StdMutex<Option<RemoteControlEnrollment>>,
    lock: Semaphore,
    lifecycle: StdMutex<CancellationToken>,
    retry_after: StdMutex<Option<time::OffsetDateTime>>,
}

impl RemoteControlEnrollmentState {
    fn new(enrollment: Option<RemoteControlEnrollment>) -> Self {
        Self {
            enrollment: StdMutex::new(enrollment),
            lock: Semaphore::new(1),
            lifecycle: StdMutex::new(CancellationToken::new()),
            retry_after: StdMutex::new(None),
        }
    }

    fn retry_after(&self) -> Option<time::OffsetDateTime> {
        self.retry_after
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .filter(|deadline| *deadline > time::OffsetDateTime::now_utc())
    }

    fn admit_request(&self) -> io::Result<()> {
        if let Some(deadline) = self.retry_after() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("remote control request deferred until {deadline}"),
            ));
        }
        Ok(())
    }

    fn record_retry_after(&self, status: axum::http::StatusCode, headers: &axum::http::HeaderMap) {
        if status != axum::http::StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
            return;
        }
        let Some(deadline) = server_api::parse_retry_after(headers, time::OffsetDateTime::now_utc())
        else {
            return;
        };
        let mut retry_after = self.retry_after.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // An older in-flight response must not shorten another response's advice.
        *retry_after = Some(retry_after.map_or(deadline, |previous| previous.max(deadline)));
    }

    async fn lock(&self) -> RemoteControlEnrollmentLease<'_> {
        let permit = match self.lock.acquire().await {
            Ok(permit) => permit,
            Err(_) => unreachable!("remote control enrollment lock should stay open"),
        };
        RemoteControlEnrollmentLease {
            state: self,
            enrollment: self.snapshot(),
            dirty: false,
            _permit: permit,
        }
    }

    fn snapshot(&self) -> Option<RemoteControlEnrollment> {
        self.enrollment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn lifecycle(&self) -> CancellationToken {
        self.lifecycle.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn retire_lifecycle(&self) {
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.cancel();
        *lifecycle = CancellationToken::new();
    }
}

struct RemoteControlEnrollmentLease<'a> {
    state: &'a RemoteControlEnrollmentState,
    enrollment: Option<RemoteControlEnrollment>,
    dirty: bool,
    _permit: SemaphorePermit<'a>,
}

impl Deref for RemoteControlEnrollmentLease<'_> {
    type Target = Option<RemoteControlEnrollment>;

    fn deref(&self) -> &Self::Target {
        &self.enrollment
    }
}

impl DerefMut for RemoteControlEnrollmentLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.dirty = true;
        &mut self.enrollment
    }
}

impl Drop for RemoteControlEnrollmentLease<'_> {
    fn drop(&mut self) {
        if !self.dirty {
            return;
        }
        *self
            .state
            .enrollment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.enrollment.take();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteControlUnavailable;

impl fmt::Display for RemoteControlUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "remote control cannot be enabled because sqlite state db is unavailable"
        )
    }
}

impl Error for RemoteControlUnavailable {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteControlDisabledByRequirements;

impl fmt::Display for RemoteControlDisabledByRequirements {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "remote control is disabled by managed requirements")
    }
}

impl Error for RemoteControlDisabledByRequirements {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteControlEnableError {
    Unavailable(RemoteControlUnavailable),
    DisabledByRequirements(RemoteControlDisabledByRequirements),
}

impl fmt::Display for RemoteControlEnableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(err) => err.fmt(f),
            Self::DisabledByRequirements(err) => err.fmt(f),
        }
    }
}

impl Error for RemoteControlEnableError {}

/// Managed-requirements decision for remote control and the policy epoch it was made for.
pub(super) type RemoteControlPolicyDecision = (RemoteControlPolicy, Option<u64>);

/// Whether `decision` admits remote control at the current policy epoch. Managed requirements
/// are fetched per account, so a decision made before the current identity completed a missing
/// account id needs a re-check first.
fn policy_admits(decision: RemoteControlPolicyDecision, identity: PolicyIdentity) -> bool {
    decision == (RemoteControlPolicy::Allowed, Some(identity.current_epoch))
}

/// Whether `decision` was made for the current authenticated identity, possibly before that
/// identity completed a missing account id within its login lifecycle.
fn policy_decided_for_identity(
    decision: RemoteControlPolicyDecision,
    identity: PolicyIdentity,
) -> bool {
    decision.1.is_some_and(|epoch| identity.includes(epoch))
}

/// Whether the decision published on `policy` admits remote control for the current identity.
fn policy_admits_current_identity(
    policy: &watch::Receiver<RemoteControlPolicyDecision>,
    auth_manager: &AuthManager,
) -> bool {
    auth_manager
        .with_policy_identity(|identity| policy_admits(*policy.borrow(), identity))
        .unwrap_or(false)
}

impl RemoteControlHandle {
    async fn run_with_lifecycle<T>(
        &self,
        lifecycle: &CancellationToken,
        operation: impl std::future::Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        tokio::select! {
            biased;
            _ = lifecycle.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "remote control operation retired by disable")),
            result = operation => {
                if lifecycle.is_cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "remote control operation retired by disable"));
                }
                result
            }
        }
    }

    pub fn ensure_remote_control_allowed(&self) -> Result<(), RemoteControlDisabledByRequirements> {
        let admitted = self
            .auth_manager
            .with_policy_identity(|identity| policy_admits(*self.policy.borrow(), identity))
            .unwrap_or(false);
        if admitted {
            Ok(())
        } else {
            Err(RemoteControlDisabledByRequirements)
        }
    }

    pub fn policy_auth_epoch(&self) -> io::Result<u64> {
        self.auth_manager
            .policy_auth_snapshot()
            .map(|(epoch, _)| epoch)
    }

    /// Records a managed-requirements decision computed at policy epoch `epoch` and ignores one
    /// computed for another identity, or for an earlier epoch of this identity than the recorded
    /// decision, so a late result cannot override a newer one. A decision computed before the
    /// current identity completed its account id cannot admit remote control until one for the
    /// current epoch replaces it, but a denial still disables remote control at once.
    pub fn update_policy(&self, epoch: u64, policy: RemoteControlPolicy) {
        let _ = self.auth_manager.with_policy_identity(|identity| {
            if !identity.includes(epoch) {
                return;
            }
            let recorded = self.policy.send_if_modified(|decision| {
                let superseded = decision
                    .1
                    .is_some_and(|decided| identity.includes(decided) && decided > epoch);
                if !superseded {
                    *decision = (policy, Some(epoch));
                }
                !superseded
            });
            if recorded && policy != RemoteControlPolicy::Allowed {
                self.transition_disabled();
            }
        });
    }

    /// Whether the managed-requirements decision predates an account-id completion of the current
    /// identity within its login lifecycle. Until `update_policy` records a decision for the
    /// current epoch, remote control rejects requests and contacts no backend, so the embedder
    /// must re-check its requirements for the completed account.
    pub fn policy_recheck_pending(&self) -> bool {
        self.auth_manager
            .with_policy_identity(|identity| {
                let decision = *self.policy.borrow();
                policy_decided_for_identity(decision, identity)
                    && decision.1 != Some(identity.current_epoch)
            })
            .unwrap_or(false)
    }

    fn ensure_remote_control_allowed_io(&self) -> io::Result<()> {
        self.ensure_remote_control_allowed()
            .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err))
    }

    // No locks span I/O. Identity/denial changes retire the operation, including
    // waiters blocked on enrollment or storage, rather than admitting its late result.
    async fn run_with_policy<T>(
        &self,
        operation: impl std::future::Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        let mut auth_changes = self.auth_manager.auth_change_receiver();
        let mut desired_changes = self.desired_state_tx.subscribe();
        let epoch = self.policy_auth_epoch()?;
        self.ensure_remote_control_allowed_io()?;
        tokio::pin!(operation);
        loop {
            tokio::select! {
                biased;
                changed = auth_changes.changed() => {
                    if changed.is_err() || self.policy_auth_epoch()? != epoch {
                        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "remote control policy identity changed"));
                    }
                    self.ensure_remote_control_allowed_io()?;
                }
                _ = desired_changes.changed() => self.ensure_remote_control_allowed_io()?,
                result = &mut operation => {
                    self.ensure_remote_control_allowed_io()?;
                    return self.auth_manager.with_policy_auth_epoch(epoch, || result)?;
                }
            }
        }
    }

    pub fn enable_ephemeral(
        &self,
    ) -> Result<RemoteControlStatusChangedNotification, RemoteControlEnableError> {
        self.enable_with_preference(/*persistence_preference*/ None)
    }

    fn enable_with_preference(
        &self,
        persistence_preference: Option<bool>,
    ) -> Result<RemoteControlStatusChangedNotification, RemoteControlEnableError> {
        self.auth_manager
            .with_policy_identity(|identity| {
                // Holding the decision orders a concurrent denial before or after this enable.
                let decision = self.policy.borrow();
                if !policy_admits(*decision, identity) {
                    return Err(RemoteControlEnableError::DisabledByRequirements(
                        RemoteControlDisabledByRequirements,
                    ));
                }

                if self.state_db.is_none() {
                    warn!(
                        "remote control cannot be enabled because sqlite state db is unavailable"
                    );
                    return Err(RemoteControlEnableError::Unavailable(
                        RemoteControlUnavailable,
                    ));
                }

                let mut effective_persistence_preference = persistence_preference;
                let desired_state_changed = self.desired_state_tx.send_if_modified(|state| {
                    if effective_persistence_preference.is_none()
                        && matches!(
                            *state,
                            RemoteControlDesiredState::Enabled {
                                persistence_preference: Some(true)
                            }
                        )
                    {
                        effective_persistence_preference = Some(true);
                    }
                    let next_state = RemoteControlDesiredState::Enabled {
                        persistence_preference: effective_persistence_preference,
                    };
                    let changed = *state != next_state;
                    *state = next_state;
                    changed
                });

                let status = self.status();
                info!(
                    desired_state_changed,
                    ?effective_persistence_preference,
                    current_status = ?status.status,
                    environment_id = ?status.environment_id,
                    installation_id = %status.installation_id,
                    server_name = %status.server_name,
                    "remote control enable requested"
                );
                if matches!(
                    status.status,
                    RemoteControlConnectionStatus::Connected
                        | RemoteControlConnectionStatus::Connecting
                ) {
                    return Ok(status);
                }

                Ok(self.publish_status(RemoteControlConnectionStatus::Connecting))
            })
            .map_err(|_| {
                RemoteControlEnableError::DisabledByRequirements(
                    RemoteControlDisabledByRequirements,
                )
            })?
    }

    pub async fn disable(
        &self,
        app_server_client_name: Option<&str>,
    ) -> io::Result<RemoteControlStatusChangedNotification> {
        self.ensure_remote_control_allowed_io()?;
        self.current_enrollment.retire_lifecycle();
        let _transition = self
            .desired_state_rpc_lock
            .acquire()
            .await
            .unwrap_or_else(|_| unreachable!());
        let _persistence = acquire_persistence_lock(&self.desired_state_persistence_lock).await;
        self.persist_preference(
            app_server_client_name,
            /*remote_control_enabled*/ false,
        )
        .await?;
        Ok(self.publish_disabled())
    }

    pub async fn disable_ephemeral(&self) -> RemoteControlStatusChangedNotification {
        self.current_enrollment.retire_lifecycle();
        let _transition = self
            .desired_state_rpc_lock
            .acquire()
            .await
            .unwrap_or_else(|_| unreachable!());
        let _persistence = acquire_persistence_lock(&self.desired_state_persistence_lock).await;
        self.publish_disabled()
    }

    fn transition_disabled(&self) -> RemoteControlStatusChangedNotification {
        self.current_enrollment.retire_lifecycle();
        self.publish_disabled()
    }

    fn publish_disabled(&self) -> RemoteControlStatusChangedNotification {
        let desired_state_changed = self.desired_state_tx.send_if_modified(|state| {
            let changed = *state != RemoteControlDesiredState::Disabled;
            *state = RemoteControlDesiredState::Disabled;
            changed
        });
        let status = self.status();
        info!(
            desired_state_changed,
            current_status = ?status.status,
            environment_id = ?status.environment_id,
            installation_id = %status.installation_id,
            server_name = %status.server_name,
            "remote control disable requested"
        );
        self.publish_status(RemoteControlConnectionStatus::Disabled)
    }

    async fn persist_preference(
        &self,
        app_server_client_name: Option<&str>,
        remote_control_enabled: bool,
    ) -> io::Result<()> {
        let state_db = self
            .state_db
            .as_deref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, RemoteControlUnavailable))?;
        let auth = load_remote_control_auth(&self.auth_manager).await?;
        let remote_control_target = normalize_remote_control_url(&self.remote_control_url)?;
        let app_server_client_name = self.pairing_persistence_key(app_server_client_name)?;
        state_db
            .set_remote_control_enabled(
                &remote_control_target.websocket_url,
                &auth.account_id,
                app_server_client_name.as_deref(),
                remote_control_enabled,
            )
            .await
            .map_err(io::Error::other)?;
        Ok(())
    }

    pub fn status(&self) -> RemoteControlStatusChangedNotification {
        self.status_tx.borrow().clone()
    }

    pub fn status_receiver(&self) -> watch::Receiver<RemoteControlStatusChangedNotification> {
        self.status_tx.subscribe()
    }

    pub async fn start_pairing(
        &self,
        params: RemoteControlPairingStartParams,
        app_server_client_name: Option<&str>,
    ) -> io::Result<RemoteControlPairingStartResponse> {
        self.run_with_policy(async {
            self.ensure_remote_control_allowed_io()?;
            if !self.desired_state_tx.borrow().is_enabled() {
                return Err(Self::pairing_disabled_error());
            }
            self.start_pairing_with_source(
                RemoteControlHttpClientSource::Pool(&self.http_clients, &self.current_enrollment),
                params,
                app_server_client_name,
            )
            .await
        })
        .await
    }


    async fn start_pairing_with_source(
        &self,
        http_clients: RemoteControlHttpClientSource<'_>,
        params: RemoteControlPairingStartParams,
        app_server_client_name: Option<&str>,
    ) -> io::Result<RemoteControlPairingStartResponse> {
        let lifecycle = self.current_enrollment.lifecycle();
        self.run_with_lifecycle(&lifecycle, self.start_pairing_inner(http_clients, params, app_server_client_name)).await
    }

    async fn start_pairing_inner(
        &self,
        http_clients: RemoteControlHttpClientSource<'_>,
        params: RemoteControlPairingStartParams,
        app_server_client_name: Option<&str>,
    ) -> io::Result<RemoteControlPairingStartResponse> {
        let mut auth = load_remote_control_auth(&self.auth_manager)
            .await
            .map_err(|_| pairing_unavailable_error())?;
        let status = self.status();
        let installation_id = status.installation_id;
        let app_server_client_name = self.pairing_persistence_key(app_server_client_name)?;
        let app_server_client_name = app_server_client_name.as_deref();
        let mut current_enrollment = self.current_enrollment.lock().await;
        let mut enrollment = self
            .load_or_enroll_pairing_server(
                http_clients,
                &mut current_enrollment,
                &mut auth,
                &installation_id,
                &status.server_name,
                app_server_client_name,
                RemoteControlEnrollmentSelection::ReuseOrCreate,
            )
            .await?;
        if enrollment.should_refresh_server_token() {
            let refresh_result = refresh_pairing_enrollment(
                http_clients,
                &mut current_enrollment,
                &self.auth_manager,
                &mut auth,
                &installation_id,
                &mut enrollment,
            )
            .await;
            if refresh_result
                .as_ref()
                .is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
            {
                enrollment = self
                    .load_or_enroll_pairing_server(
                        http_clients,
                        &mut current_enrollment,
                        &mut auth,
                        &installation_id,
                        &status.server_name,
                        app_server_client_name,
                        RemoteControlEnrollmentSelection::ReplaceExisting,
                    )
                    .await?;
            } else {
                refresh_result?;
            }
        }
        let pairing_request = || protocol::StartRemoteControlPairingRequest {
            manual_code: params.manual_code,
        };
        // Ordinary pairing HTTP neither creates nor mutates enrollment. Release
        // ownership and validate the exact identity/token when admitting its result.
        drop(current_enrollment);
        let response = start_pairing_request(http_clients, &enrollment, pairing_request()).await;
        let mut current_enrollment = self.current_enrollment.lock().await;
        let current = current_enrollment.as_ref()
            .filter(|current| same_remote_control_enrollment(current, &enrollment))
            .ok_or_else(pairing_unavailable_error)?;
        let mut token_changed = current.remote_control_token != enrollment.remote_control_token;
        enrollment = current.clone();
        let pairing_response =
            match response {
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                    if !token_changed {
                        clear_pairing_server_token(&mut current_enrollment, &mut enrollment)?;
                    }
                    refresh_pairing_enrollment(
                        http_clients,
                        &mut current_enrollment,
                        &self.auth_manager,
                        &mut auth,
                        &installation_id,
                        &mut enrollment,
                    )
                    .await?;
                    let expected = enrollment.clone();
                    drop(current_enrollment);
                    let response = start_pairing_request(http_clients, &enrollment, pairing_request()).await;
                    (current_enrollment, enrollment, token_changed) = self.pairing_response_enrollment(&expected).await?;
                    response
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // A delayed response for an older bearer cannot invalidate
                    // a newer generation installed while HTTP ran without the lease.
                    if !token_changed {
                        enrollment = self
                        .load_or_enroll_pairing_server(
                            http_clients,
                            &mut current_enrollment,
                            &mut auth,
                            &installation_id,
                            &status.server_name,
                            app_server_client_name,
                            RemoteControlEnrollmentSelection::ReplaceExisting,
                        )
                        .await?;
                    }
                    let expected = enrollment.clone();
                    drop(current_enrollment);
                    let response = start_pairing_request(http_clients, &enrollment, pairing_request()).await;
                    (current_enrollment, enrollment, token_changed) = self.pairing_response_enrollment(&expected).await?;
                    response
                }
                pairing_response => pairing_response,
            };
        if let Err(err) = &pairing_response {
            match err.kind() {
                io::ErrorKind::NotFound => {
                    return Err(pairing_unavailable_error());
                }
                io::ErrorKind::PermissionDenied => {
                    if !token_changed {
                        clear_pairing_server_token(&mut current_enrollment, &mut enrollment)?;
                    }
                    return Err(pairing_unavailable_error());
                }
                _ => {}
            }
        }
        let current_auth = load_remote_control_auth(&self.auth_manager)
            .await
            .map_err(|_| pairing_unavailable_error())?;
        if current_auth.account_id != auth.account_id {
            return Err(pairing_unavailable_error());
        }
        if !self.desired_state_tx.borrow().is_enabled() {
            return Err(Self::pairing_disabled_error());
        }
        pairing_response
    }

    // Pairing enrollment intentionally carries the selected server and persistence context.
    #[allow(clippy::too_many_arguments)]
    async fn load_or_enroll_pairing_server(
        &self,
        http_clients: RemoteControlHttpClientSource<'_>,
        current_enrollment: &mut Option<RemoteControlEnrollment>,
        auth: &mut auth::RemoteControlConnectionAuth,
        installation_id: &str,
        server_name: &str,
        app_server_client_name: Option<&str>,
        selection: RemoteControlEnrollmentSelection,
    ) -> io::Result<RemoteControlEnrollment> {
        let (enrollment, created) = self
            .load_or_enroll_server_with_source(
                http_clients,
                current_enrollment,
                auth,
                installation_id,
                server_name,
                app_server_client_name,
                selection,
            )
            .await?;
        if !created {
            publish_current_enrollment(current_enrollment, &enrollment);
            return Ok(enrollment);
        }

        let state_db = self
            .state_db
            .as_deref()
            .ok_or_else(pairing_unavailable_error)?;
        let _persistence = acquire_persistence_lock(&self.desired_state_persistence_lock).await;
        let persistence_preference = match *self.desired_state_tx.borrow() {
            RemoteControlDesiredState::Enabled {
                persistence_preference,
            } => persistence_preference,
            RemoteControlDesiredState::Unknown | RemoteControlDesiredState::Disabled => {
                return Err(Self::pairing_disabled_error());
            }
        };
        update_persisted_remote_control_enrollment(
            Some(state_db),
            &enrollment.remote_control_target,
            &auth.account_id,
            app_server_client_name,
            Some(&enrollment),
            persistence_preference,
        )
        .await?;
        publish_current_enrollment(current_enrollment, &enrollment);
        Ok(enrollment)
    }

    // Server selection must keep all enrollment identity inputs in one atomic decision.
    #[allow(clippy::too_many_arguments)]
    async fn load_or_enroll_server_with_source(
        &self,
        http_clients: RemoteControlHttpClientSource<'_>,
        current_enrollment: &Option<RemoteControlEnrollment>,
        auth: &mut auth::RemoteControlConnectionAuth,
        installation_id: &str,
        server_name: &str,
        app_server_client_name: Option<&str>,
        selection: RemoteControlEnrollmentSelection,
    ) -> io::Result<(RemoteControlEnrollment, bool)> {
        let remote_control_target = normalize_remote_control_url(&self.remote_control_url)?;
        match selection {
            RemoteControlEnrollmentSelection::ReuseOrCreate => {
                if let Some(enrollment) = current_enrollment
                    .as_ref()
                    .filter(|enrollment| enrollment.account_id == auth.account_id)
                    .cloned()
                {
                    return Ok((enrollment, false));
                }

                let state_db = self
                    .state_db
                    .as_deref()
                    .ok_or_else(pairing_unavailable_error)?;
                if let Some(mut enrollment) = load_persisted_remote_control_enrollment(
                    Some(state_db),
                    &remote_control_target,
                    &auth.account_id,
                    app_server_client_name,
                )
                .await?
                {
                    enrollment.server_name = server_name.to_string();
                    return Ok((enrollment, false));
                }
            }
            RemoteControlEnrollmentSelection::ReplaceExisting => {}
        }

        let enrollment = enroll_pairing_server(
            http_clients,
            &self.auth_manager,
            auth,
            &remote_control_target,
            installation_id,
            server_name,
        )
        .await?;
        Ok((enrollment, true))
    }

    fn pairing_persistence_key(
        &self,
        app_server_client_name: Option<&str>,
    ) -> io::Result<Option<String>> {
        if self.pairing_persistence_key_required && self.pairing_persistence_key.borrow().is_none()
        {
            let app_server_client_name =
                app_server_client_name.ok_or_else(pairing_unavailable_error)?;
            self.pairing_persistence_key
                .send_replace(Some(app_server_client_name.to_string()));
        }
        Ok(self.pairing_persistence_key.borrow().clone())
    }

    pub async fn pairing_status(
        &self,
        params: RemoteControlPairingStatusParams,
    ) -> io::Result<RemoteControlPairingStatusResponse> {
        self.run_with_policy(async {
            self.ensure_remote_control_allowed_io()?;
            if !self.desired_state_tx.borrow().is_enabled() {
                return Err(Self::pairing_disabled_error());
            }
            self.pairing_status_with_source(
                RemoteControlHttpClientSource::Pool(&self.http_clients, &self.current_enrollment),
                params,
            )
            .await
        })
        .await
    }

    async fn pairing_status_with_source(
        &self,
        http_clients: RemoteControlHttpClientSource<'_>,
        params: RemoteControlPairingStatusParams,
    ) -> io::Result<RemoteControlPairingStatusResponse> {
        let lifecycle = self.current_enrollment.lifecycle();
        self.run_with_lifecycle(&lifecycle, self.pairing_status_inner(http_clients, params)).await
    }

    async fn pairing_status_inner(
        &self,
        http_clients: RemoteControlHttpClientSource<'_>,
        params: RemoteControlPairingStatusParams,
    ) -> io::Result<RemoteControlPairingStatusResponse> {
        let status_code = remote_control_pairing_status_code(&params)?;
        let mut auth = load_remote_control_auth(&self.auth_manager)
            .await
            .map_err(|_| pairing_unavailable_error())?;
        let app_server_client_name = self.pairing_persistence_key.borrow().clone();
        let app_server_client_name = app_server_client_name.as_deref();
        let mut current_enrollment = self.current_enrollment.lock().await;
        let mut enrollment = current_enrollment
            .as_ref()
            .filter(|enrollment| enrollment.account_id == auth.account_id)
            .cloned()
            .ok_or_else(pairing_unavailable_error)?;
        let status = self.status();
        let installation_id = status.installation_id;
        let server_name = status.server_name;
        if enrollment.should_refresh_server_token() {
            let refresh_result = refresh_pairing_enrollment(
                http_clients,
                &mut current_enrollment,
                &self.auth_manager,
                &mut auth,
                &installation_id,
                &mut enrollment,
            )
            .await;
            if refresh_result
                .as_ref()
                .is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
            {
                self.load_or_enroll_pairing_server(
                    http_clients,
                    &mut current_enrollment,
                    &mut auth,
                    &installation_id,
                    &server_name,
                    app_server_client_name,
                    RemoteControlEnrollmentSelection::ReplaceExisting,
                )
                .await?;
                return Err(pairing_unavailable_error());
            }
            refresh_result?;
        }
        let pairing_status_request =
            || protocol::RemoteControlPairingStatusRequest::from(status_code.clone());
        drop(current_enrollment);
        let response = pairing_status_request_with_source(
            http_clients,
            &enrollment,
            pairing_status_request(),
        )
        .await;
        let mut current_enrollment = self.current_enrollment.lock().await;
        let current = current_enrollment.as_ref()
            .filter(|current| same_remote_control_enrollment(current, &enrollment))
            .ok_or_else(pairing_unavailable_error)?;
        let mut token_changed = current.remote_control_token != enrollment.remote_control_token;
        enrollment = current.clone();
        let pairing_status_response = match response {
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                if !token_changed {
                    clear_pairing_server_token(&mut current_enrollment, &mut enrollment)?;
                }
                refresh_pairing_enrollment(
                    http_clients,
                    &mut current_enrollment,
                    &self.auth_manager,
                    &mut auth,
                    &installation_id,
                    &mut enrollment,
                )
                .await?;
                let expected = enrollment.clone();
                drop(current_enrollment);
                let response = pairing_status_request_with_source(
                    http_clients,
                    &enrollment,
                    pairing_status_request(),
                )
                .await;
                (current_enrollment, enrollment, token_changed) = self.pairing_response_enrollment(&expected).await?;
                response
            }
            pairing_status_response => pairing_status_response,
        };
        if let Err(err) = &pairing_status_response {
            match err.kind() {
                io::ErrorKind::NotFound => {
                    if token_changed { return Err(pairing_unavailable_error()); }
                    self.load_or_enroll_pairing_server(
                        http_clients,
                        &mut current_enrollment,
                        &mut auth,
                        &installation_id,
                        &server_name,
                        app_server_client_name,
                        RemoteControlEnrollmentSelection::ReplaceExisting,
                    )
                    .await?;
                    return Err(pairing_unavailable_error());
                }
                io::ErrorKind::PermissionDenied => {
                    if !token_changed {
                        clear_pairing_server_token(&mut current_enrollment, &mut enrollment)?;
                    }
                    return Err(pairing_unavailable_error());
                }
                _ => {}
            }
        }
        if !self.desired_state_tx.borrow().is_enabled() {
            return Err(Self::pairing_disabled_error());
        }
        let current_auth = load_remote_control_auth(&self.auth_manager)
            .await
            .map_err(|_| pairing_unavailable_error())?;
        if current_auth.account_id != auth.account_id {
            return Err(pairing_unavailable_error());
        }
        pairing_status_response
    }

    async fn pairing_response_enrollment(
        &self,
        expected: &RemoteControlEnrollment,
    ) -> io::Result<(RemoteControlEnrollmentLease<'_>, RemoteControlEnrollment, bool)> {
        let lease = self.current_enrollment.lock().await;
        let current = lease.as_ref()
            .filter(|current| same_remote_control_enrollment(current, expected))
            .ok_or_else(pairing_unavailable_error)?;
        let token_changed = current.remote_control_token != expected.remote_control_token;
        let enrollment = current.clone();
        Ok((lease, enrollment, token_changed))
    }

    pub async fn list_clients(
        &self,
        params: RemoteControlClientsListParams,
    ) -> io::Result<RemoteControlClientsListResponse> {
        self.run_with_policy(async {
            self.ensure_remote_control_allowed_io()?;
            clients::list_remote_control_clients_with_pool(
                &self.http_clients,
                &self.current_enrollment,
                &self.remote_control_url,
                &self.auth_manager,
                params,
            )
            .await
        })
        .await
    }

    pub async fn revoke_client(
        &self,
        params: RemoteControlClientsRevokeParams,
    ) -> io::Result<RemoteControlClientsRevokeResponse> {
        self.run_with_policy(async {
            self.ensure_remote_control_allowed_io()?;
            clients::revoke_remote_control_client_with_pool(
                &self.http_clients,
                &self.current_enrollment,
                &self.remote_control_url,
                &self.auth_manager,
                params,
            )
            .await
        })
        .await
    }

    fn pairing_disabled_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote control pairing requires remote control to be enabled",
        )
    }

    fn publish_status(
        &self,
        connection_status: RemoteControlConnectionStatus,
    ) -> RemoteControlStatusChangedNotification {
        let mut status_change = None;
        self.status_tx.send_if_modified(|status| {
            let next_status =
                remote_control_status_with_connection_status(status, connection_status);
            if *status == next_status {
                return false;
            }

            status_change = Some((status.clone(), next_status.clone()));
            *status = next_status;
            true
        });
        if let Some((previous_status, next_status)) = status_change {
            info!(
                previous_status = ?previous_status.status,
                next_status = ?next_status.status,
                previous_environment_id = ?previous_status.environment_id,
                next_environment_id = ?next_status.environment_id,
                installation_id = %next_status.installation_id,
                server_name = %next_status.server_name,
                "remote control handle status changed"
            );
        }
        self.status()
    }
}

async fn enroll_pairing_server(
    http_clients: RemoteControlHttpClientSource<'_>,
    auth_manager: &Arc<AuthManager>,
    auth: &mut auth::RemoteControlConnectionAuth,
    remote_control_target: &protocol::RemoteControlTarget,
    installation_id: &str,
    server_name: &str,
) -> io::Result<RemoteControlEnrollment> {
    let client = http_clients
        .client_for_url(&remote_control_target.enroll_url)
        .await?;
    match enroll_remote_control_server_with_client(
        &client,
        http_clients.request_state(),
        remote_control_target,
        auth,
        installation_id,
        server_name,
    )
    .await
    {
        Ok(enrollment) => return Ok(enrollment),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            let mut auth_recovery = auth_manager.unauthorized_recovery();
            let mut auth_change_rx = auth_manager.auth_change_receiver();
            if !recover_remote_control_auth(&mut auth_recovery, &mut auth_change_rx, auth).await {
                return Err(err);
            }
            *auth = load_remote_control_auth(auth_manager)
                .await
                .map_err(|_| pairing_unavailable_error())?;
        }
        Err(err) => return Err(err),
    }
    enroll_remote_control_server_with_client(
        &client,
        http_clients.request_state(),
        remote_control_target,
        auth,
        installation_id,
        server_name,
    )
    .await
}

fn remote_control_pairing_status_code(
    params: &RemoteControlPairingStatusParams,
) -> io::Result<RemoteControlPairingStatusCode> {
    match (&params.pairing_code, &params.manual_pairing_code) {
        (Some(pairing_code), None) => Ok(RemoteControlPairingStatusCode::PairingCode(
            pairing_code.clone(),
        )),
        (None, Some(manual_pairing_code)) => Ok(RemoteControlPairingStatusCode::ManualPairingCode(
            manual_pairing_code.clone(),
        )),
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote control pairing status accepts either pairingCode or manualPairingCode, not both",
        )),
        (None, None) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote control pairing status requires pairingCode or manualPairingCode",
        )),
    }
}

async fn refresh_pairing_enrollment(
    http_clients: RemoteControlHttpClientSource<'_>,
    current_enrollment: &mut Option<RemoteControlEnrollment>,
    auth_manager: &Arc<AuthManager>,
    auth: &mut auth::RemoteControlConnectionAuth,
    installation_id: &str,
    enrollment: &mut RemoteControlEnrollment,
) -> io::Result<()> {
    let client = http_clients
        .client_for_url(&enrollment.remote_control_target.refresh_url)
        .await?;
    let mut refresh_result = refresh_remote_control_server_with_client(
        &client,
        http_clients.request_state(),
        auth,
        installation_id,
        enrollment,
    )
    .await;
    if refresh_result
        .as_ref()
        .is_err_and(|err| err.kind() == io::ErrorKind::PermissionDenied)
    {
        let mut auth_recovery = auth_manager.unauthorized_recovery();
        let mut auth_change_rx = auth_manager.auth_change_receiver();
        if recover_remote_control_auth(&mut auth_recovery, &mut auth_change_rx, auth).await {
            match load_remote_control_auth(auth_manager).await {
                Ok(recovered_auth) if recovered_auth.account_id == enrollment.account_id => {
                    *auth = recovered_auth;
                    refresh_result = refresh_remote_control_server_with_client(
                        &client,
                        http_clients.request_state(),
                        auth,
                        installation_id,
                        enrollment,
                    )
                    .await;
                }
                Ok(_) | Err(_) => {
                    enrollment.clear_server_token();
                    refresh_result = Err(pairing_unavailable_error());
                }
            }
        } else {
            enrollment.clear_server_token();
        }
    }
    if refresh_result
        .as_ref()
        .is_err_and(|err| err.kind() == io::ErrorKind::PermissionDenied)
    {
        enrollment.clear_server_token();
    }
    if !replace_current_enrollment(current_enrollment, enrollment) {
        Err(pairing_unavailable_error())
    } else {
        refresh_result
    }
}

async fn start_pairing_request(
    http_clients: RemoteControlHttpClientSource<'_>,
    enrollment: &RemoteControlEnrollment,
    request: protocol::StartRemoteControlPairingRequest,
) -> io::Result<RemoteControlPairingStartResponse> {
    let client = http_clients
        .client_for_url(&enrollment.remote_control_target.pair_url)
        .await?;
    enrollment
        .start_pairing_with_client(&client, http_clients.request_state(), request)
        .await
}

async fn pairing_status_request_with_source(
    http_clients: RemoteControlHttpClientSource<'_>,
    enrollment: &RemoteControlEnrollment,
    request: protocol::RemoteControlPairingStatusRequest,
) -> io::Result<RemoteControlPairingStatusResponse> {
    let client = http_clients
        .client_for_url(&enrollment.remote_control_target.pair_status_url)
        .await?;
    enrollment
        .pairing_status_with_client(&client, http_clients.request_state(), request)
        .await
}

fn clear_pairing_server_token(
    current_enrollment: &mut Option<RemoteControlEnrollment>,
    enrollment: &mut RemoteControlEnrollment,
) -> io::Result<()> {
    enrollment.clear_server_token();
    if replace_current_enrollment(current_enrollment, enrollment) {
        Ok(())
    } else {
        Err(pairing_unavailable_error())
    }
}

fn pairing_unavailable_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "remote control pairing is unavailable until enrollment completes",
    )
}

fn remote_control_status_with_connection_status(
    status: &RemoteControlStatusChangedNotification,
    connection_status: RemoteControlConnectionStatus,
) -> RemoteControlStatusChangedNotification {
    RemoteControlStatusChangedNotification {
        status: connection_status,
        server_name: status.server_name.clone(),
        installation_id: status.installation_id.clone(),
        environment_id: if connection_status == RemoteControlConnectionStatus::Disabled {
            None
        } else {
            status.environment_id.clone()
        },
    }
}

fn publish_current_enrollment(
    current_enrollment: &mut Option<RemoteControlEnrollment>,
    enrollment: &RemoteControlEnrollment,
) {
    *current_enrollment = Some(enrollment.clone());
}

fn replace_current_enrollment(
    current_enrollment: &mut Option<RemoteControlEnrollment>,
    enrollment: &RemoteControlEnrollment,
) -> bool {
    if !current_enrollment
        .as_ref()
        .is_some_and(|current| same_remote_control_enrollment(current, enrollment))
    {
        return false;
    }
    *current_enrollment = Some(enrollment.clone());
    true
}

fn same_remote_control_enrollment(
    left: &RemoteControlEnrollment,
    right: &RemoteControlEnrollment,
) -> bool {
    // A refresh rotates only the bearer. Pairing remains current while the same persisted server
    // record is still selected for the current account.
    left.account_id == right.account_id
        && left.remote_control_target == right.remote_control_target
        && left.server_id == right.server_id
        && left.environment_id == right.environment_id
}

pub async fn start_remote_control(
    config: RemoteControlStartConfig,
    state_db: Option<Arc<StateRuntime>>,
    auth_manager: Arc<AuthManager>,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
    app_server_client_name_rx: Option<oneshot::Receiver<String>>,
    startup_mode: RemoteControlStartupMode,
) -> io::Result<(JoinHandle<()>, RemoteControlHandle)> {
    let http_clients = remote_control_http_clients(config.http_client_factory.clone());
    let policy = config.policy;
    let policy_epoch = auth_manager
        .policy_auth_snapshot()
        .ok()
        .map(|(epoch, _)| epoch);
    let (shared_policy, websocket_policy) = watch::channel((policy, policy_epoch));
    let shared_policy = Arc::new(shared_policy);
    let monitored_policy = shared_policy.clone();
    let policy_auth_manager = auth_manager.clone();
    let mut policy_auth_changes = auth_manager.auth_change_receiver();
    let state_db_available = state_db.is_some();
    let requested_initial_enabled = startup_mode == RemoteControlStartupMode::EnabledEphemeral;
    let desired_state =
        if policy == RemoteControlPolicy::DisabledByRequirements || !state_db_available {
            RemoteControlDesiredState::Disabled
        } else {
            match startup_mode {
                RemoteControlStartupMode::ResolvePersisted => RemoteControlDesiredState::Unknown,
                RemoteControlStartupMode::DisabledEphemeral => RemoteControlDesiredState::Disabled,
                RemoteControlStartupMode::EnabledEphemeral => RemoteControlDesiredState::Enabled {
                    persistence_preference: None,
                },
            }
        };
    let initial_enabled = desired_state.is_enabled();
    if requested_initial_enabled && !state_db_available {
        warn!("remote control disabled because sqlite state db is unavailable");
    }
    let remote_control_target = if initial_enabled {
        Some(normalize_remote_control_url(&config.remote_control_url)?)
    } else {
        None
    };

    let (desired_state_tx, _desired_state_rx) = watch::channel(desired_state);
    let desired_state_tx = Arc::new(desired_state_tx);
    let desired_state_rpc_lock = Arc::new(Semaphore::new(1));
    let desired_state_persistence_lock = Arc::new(Semaphore::new(1));
    let websocket_desired_state_tx = desired_state_tx.clone();
    let policy_desired_state_tx = desired_state_tx.clone();
    let websocket_desired_state_persistence_lock = desired_state_persistence_lock.clone();
    let current_enrollment = Arc::new(RemoteControlEnrollmentState::new(/*enrollment*/ None));
    let websocket_current_enrollment = current_enrollment.clone();
    let pairing_persistence_key_required = app_server_client_name_rx.is_some();
    let (pairing_persistence_key, _pairing_persistence_key_rx) = watch::channel(None);
    let websocket_pairing_persistence_key = pairing_persistence_key.clone();
    let handle_auth_manager = auth_manager.clone();
    let handle_state_db = state_db.clone();
    let server_name = gethostname().to_string_lossy().trim().to_string();
    let remote_control_url = config.remote_control_url;
    let installation_id = config.installation_id;
    let initial_status = RemoteControlStatusChangedNotification {
        status: if initial_enabled {
            RemoteControlConnectionStatus::Connecting
        } else {
            RemoteControlConnectionStatus::Disabled
        },
        server_name: server_name.clone(),
        installation_id: installation_id.clone(),
        environment_id: None,
    };
    let (status_tx, _status_rx) = watch::channel(initial_status);
    let status_publisher = RemoteControlStatusPublisher::new(status_tx.clone());
    info!(
        remote_control_url = %remote_control_url,
        installation_id = %installation_id,
        server_name = %server_name,
        state_db_available,
        ?desired_state,
        "starting app-server remote control websocket task"
    );
    let remote_control_url_for_log = remote_control_url.clone();
    let handle_remote_control_url = remote_control_url.clone();
    let installation_id_for_log = installation_id.clone();
    let server_name_for_log = server_name.clone();
    let shutdown_token_for_log = shutdown_token.clone();
    let websocket_http_clients = http_clients.clone();
    let join_handle = tokio::spawn(async move {
        info!(
            remote_control_url = %remote_control_url_for_log,
            installation_id = %installation_id_for_log,
            server_name = %server_name_for_log,
            ?desired_state,
            "app-server remote control websocket task started"
        );
        let websocket_task = RemoteControlWebsocket::new(
            websocket::RemoteControlWebsocketConfig {
                remote_control_url,
                installation_id,
                remote_control_target,
                server_name,
                http_clients: websocket_http_clients,
            },
            state_db,
            auth_manager,
            RemoteControlChannels {
                transport_event_tx,
                status_publisher,
                current_enrollment: websocket_current_enrollment,
                pairing_persistence_key: websocket_pairing_persistence_key,
                desired_state_persistence_lock: websocket_desired_state_persistence_lock,
            },
            shutdown_token,
            websocket_desired_state_tx,
        )
        .with_policy(websocket_policy)
        .run(app_server_client_name_rx);
        let websocket_task = AssertUnwindSafe(websocket_task).catch_unwind();
        tokio::pin!(websocket_task);
        let outcome = loop {
            tokio::select! {
                biased;
                changed = policy_auth_changes.changed() => {
                    if changed.is_ok() {
                        // A completed account id keeps its identity's decision, held for a
                        // requirements re-check. Any other identity change disables remote
                        // control until a decision for the new identity is recorded.
                        let same_identity = policy_auth_manager
                            .with_policy_identity(|identity| {
                                policy_decided_for_identity(*monitored_policy.borrow(), identity)
                            })
                            .unwrap_or(false);
                        if !same_identity {
                            policy_desired_state_tx.send_replace(RemoteControlDesiredState::Disabled);
                        }
                    }
                }
                outcome = &mut websocket_task => break outcome,
            }
        };
        match outcome {
            Ok(()) => {
                let shutdown_requested = shutdown_token_for_log.is_cancelled();
                if shutdown_requested {
                    info!(
                        remote_control_url = %remote_control_url_for_log,
                        installation_id = %installation_id_for_log,
                        server_name = %server_name_for_log,
                        shutdown_requested,
                        "app-server remote control websocket task exited"
                    );
                } else {
                    warn!(
                        remote_control_url = %remote_control_url_for_log,
                        installation_id = %installation_id_for_log,
                        server_name = %server_name_for_log,
                        shutdown_requested,
                        "app-server remote control websocket task exited without shutdown"
                    );
                }
            }
            Err(panic) => {
                error!(
                    remote_control_url = %remote_control_url_for_log,
                    installation_id = %installation_id_for_log,
                    server_name = %server_name_for_log,
                    "app-server remote control websocket task panicked"
                );
                std::panic::resume_unwind(panic);
            }
        }
    });

    Ok((
        join_handle,
        RemoteControlHandle {
            policy: shared_policy,
            desired_state_tx,
            desired_state_rpc_lock,
            desired_state_persistence_lock,
            status_tx: Arc::new(status_tx),
            state_db: handle_state_db,
            remote_control_url: handle_remote_control_url,
            current_enrollment,
            pairing_persistence_key,
            pairing_persistence_key_required,
            auth_manager: handle_auth_manager,
            http_clients,
        },
    ))
}

