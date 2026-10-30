use indexmap::IndexMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use uuid::Uuid;

use crate::ExecServerError;
use crate::ExecServerRuntimePaths;
use crate::ExecutorFileSystem;
use crate::HttpClient;
use crate::NoiseChannelIdentity;
use crate::NoiseRendezvousConnectProvider;
use crate::client::LazyRemoteExecServerClient;
use crate::client::http_client::ReqwestHttpClient;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_api::ExecServerTransportParams;
use crate::environment_provider::DefaultEnvironmentProvider;
use crate::environment_provider::EnvironmentDefault;
use crate::environment_provider::EnvironmentProvider;
use crate::environment_provider::EnvironmentProviderSnapshot;
use crate::environment_provider::normalize_exec_server_url;
use crate::environment_toml::environment_provider_from_codex_home;
use crate::local_file_system::LocalFileSystem;
use crate::local_process::LocalProcess;
use crate::process::ExecBackend;
use crate::protocol::EnvironmentInfo;
use crate::remote::NoiseRendezvousEnvironmentConfig;
use crate::remote_file_system::RemoteFileSystem;
use crate::remote_process::RemoteProcess;
use tokio_util::task::AbortOnDropHandle;

pub const CODEX_EXEC_SERVER_URL_ENV_VAR: &str = "CODEX_EXEC_SERVER_URL";
pub const CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_REGISTRY_URL";
pub const CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID";
pub const CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR: &str = "CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN";
pub const CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID";

/// Owns the execution/filesystem environments available to the Codex runtime.
///
/// `EnvironmentManager` is a shared registry for concrete environments. Its
/// default constructor preserves the legacy `CODEX_EXEC_SERVER_URL` behavior
/// while configured construction accepts a provider-supplied snapshot.
///
/// Setting `CODEX_EXEC_SERVER_URL=none` disables environment access by leaving
/// the default environment unset and omitting the local environment. Callers
/// use `default_environment().is_some()` as the signal for model-facing
/// shell/filesystem tool availability.
///
/// Remote environments begin connecting when added to the manager. Their
/// filesystem and execution backends share that startup result and reconnect
/// after later disconnects as needed.
#[derive(Debug)]
pub struct EnvironmentManager {
    default_environment: Option<String>,
    pub(super) environments: RwLock<IndexMap<String, Arc<Environment>>>,
    local_environment: Option<Arc<Environment>>,
    local_runtime_paths: Option<ExecServerRuntimePaths>,
    http_client_factory: codex_http_client::HttpClientFactory,
}

pub const LOCAL_ENVIRONMENT_ID: &str = "local";
pub const REMOTE_ENVIRONMENT_ID: &str = "remote";

impl EnvironmentManager {


    /// Builds a manager with no configured execution environments.
    pub fn without_environments() -> Self {
        Self {
            default_environment: None,
            environments: RwLock::new(IndexMap::new()),
            local_environment: None,
            local_runtime_paths: None,
            http_client_factory: default_http_client_factory(),
        }
    }


    /// Builds a manager from `CODEX_HOME` and local runtime paths used when
    /// creating local filesystem helpers.
    ///
    /// If `CODEX_HOME/environments.toml` is present, it defines the configured
    /// environments. Otherwise this preserves the legacy
    /// `CODEX_EXEC_SERVER_URL` behavior.
    pub async fn from_codex_home(
        codex_home: impl AsRef<std::path::Path>,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Result<Self, ExecServerError> {
        Self::from_codex_home_with_http_client_factory(codex_home, local_runtime_paths, default_http_client_factory()).await
    }

    pub async fn from_codex_home_with_http_client_factory(
        codex_home: impl AsRef<std::path::Path>,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        if let Some(config) = noise_environment_config_from_env(&http_client_factory)? {
            return Self::from_noise_environment_config(config, local_runtime_paths, http_client_factory);
        }
        let codex_home = codex_home.as_ref().to_path_buf();
        let provider =
            tokio::task::spawn_blocking(move || environment_provider_from_codex_home(&codex_home))
                .await
                .map_err(|error| {
                    ExecServerError::Protocol(format!(
                        "environment config discovery task failed: {error}"
                    ))
                })??;
        Self::from_snapshot(provider.snapshot().await?, local_runtime_paths, http_client_factory)
    }

    /// Builds a manager from the legacy environment-variable provider without
    /// reading user config files from `CODEX_HOME`.
    pub async fn from_env(
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Result<Self, ExecServerError> {
        Self::from_env_with_http_client_factory(local_runtime_paths, default_http_client_factory()).await
    }

    pub async fn from_env_with_http_client_factory(
        local_runtime_paths: Option<ExecServerRuntimePaths>,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        if let Some(config) = noise_environment_config_from_env(&http_client_factory)? {
            return Self::from_noise_environment_config(config, local_runtime_paths, http_client_factory);
        }
        let provider = DefaultEnvironmentProvider::from_env();
        Self::from_snapshot(provider.snapshot().await?, local_runtime_paths, http_client_factory)
    }


    fn from_noise_environment_config(
        config: NoiseRendezvousEnvironmentConfig,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let manager = Self {
            default_environment: Some(REMOTE_ENVIRONMENT_ID.to_string()),
            environments: RwLock::new(IndexMap::new()),
            local_environment: None,
            local_runtime_paths,
            http_client_factory,
        };
        manager.upsert_noise_environment(
            REMOTE_ENVIRONMENT_ID.to_string(),
            config.connect_provider(),
        )?;
        Ok(manager)
    }


    fn from_snapshot(
        snapshot: EnvironmentProviderSnapshot,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let mut environment_map =
            IndexMap::with_capacity(environments.len() + usize::from(include_local));
        let local_environment = if include_local {
            let local_runtime_paths = local_runtime_paths.clone().ok_or_else(|| {
                ExecServerError::Protocol(
                    "local environment requires configured runtime paths".to_string(),
                )
            })?;
            let local_environment = Arc::new(Environment::local(local_runtime_paths));
            environment_map.insert(
                LOCAL_ENVIRONMENT_ID.to_string(),
                Arc::clone(&local_environment),
            );
            Some(local_environment)
        } else {
            None
        };
        for (id, environment) in environments {
            if id.is_empty() {
                return Err(ExecServerError::Protocol(
                    "environment id cannot be empty".to_string(),
                ));
            }
            if id == LOCAL_ENVIRONMENT_ID {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{LOCAL_ENVIRONMENT_ID}` is reserved for EnvironmentManager"
                )));
            }
            if environment_map
                .insert(id.clone(), Arc::new(environment.with_http_client_factory(http_client_factory.clone())))
                .is_some()
            {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{id}` is duplicated"
                )));
            }
        }
        let default_environment = match default {
            EnvironmentDefault::Disabled => None,
            EnvironmentDefault::EnvironmentId(environment_id) => {
                if !environment_map.contains_key(&environment_id) {
                    return Err(ExecServerError::Protocol(format!(
                        "default environment `{environment_id}` is not configured"
                    )));
                }
                Some(environment_id)
            }
        };
        // The snapshot is valid; start connecting its remote environments in the background.
        for environment in environment_map.values() {
            environment.start_connecting();
        }
        Ok(Self {
            default_environment,
            environments: RwLock::new(environment_map),
            local_environment,
            local_runtime_paths,
            http_client_factory,
        })
    }

    /// Returns the default environment instance.
    pub fn default_environment(&self) -> Option<Arc<Environment>> {
        self.default_environment
            .as_deref()
            .and_then(|environment_id| self.get_environment(environment_id))
    }

    /// Returns the id of the default environment.
    pub fn default_environment_id(&self) -> Option<&str> {
        self.default_environment.as_deref()
    }

    /// Returns the ordered environment ids used for new thread startup.
    pub fn default_environment_ids(&self) -> Vec<String> {
        let Some(default_environment_id) = self.default_environment.as_ref() else {
            return Vec::new();
        };
        let environments = self
            .environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut environment_ids = Vec::with_capacity(environments.len());
        environment_ids.push(default_environment_id.clone());
        environment_ids.extend(
            environments
                .keys()
                .filter(|environment_id| *environment_id != default_environment_id)
                .cloned(),
        );
        environment_ids
    }

    /// Returns the local environment instance when one is configured.
    pub fn try_local_environment(&self) -> Option<Arc<Environment>> {
        self.local_environment.as_ref().map(Arc::clone)
    }

    /// Returns the default environment or local environment when either exists.
    pub fn default_or_local_environment(&self) -> Option<Arc<Environment>> {
        self.default_environment()
            .or_else(|| self.try_local_environment())
    }

    /// Returns a named environment instance.
    pub fn get_environment(&self, environment_id: &str) -> Option<Arc<Environment>> {
        self.environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(environment_id)
            .cloned()
    }

    /// Adds or replaces a named remote environment without changing the
    /// manager's default environment selection. Uses the default WebSocket
    /// connection timeout when none is provided.
    pub fn upsert_environment(
        &self,
        environment_id: String,
        exec_server_url: String,
        connect_timeout: Option<std::time::Duration>,
    ) -> Result<(), ExecServerError> {
        if environment_id == LOCAL_ENVIRONMENT_ID {
            return Err(ExecServerError::Protocol(format!(
                "environment id `{LOCAL_ENVIRONMENT_ID}` is reserved for EnvironmentManager"
            )));
        }
        if environment_id.is_empty() {
            return Err(ExecServerError::Protocol(
                "environment id cannot be empty".to_string(),
            ));
        }
        let (exec_server_url, disabled) = normalize_exec_server_url(Some(exec_server_url));
        if disabled {
            return Err(ExecServerError::Protocol(
                "remote environment cannot use disabled exec-server url".to_string(),
            ));
        }
        let Some(exec_server_url) = exec_server_url else {
            return Err(ExecServerError::Protocol(
                "remote environment requires an exec-server url".to_string(),
            ));
        };
        let environment = Arc::new(Environment::remote_with_transport(
            ExecServerTransportParams::websocket_url(
                exec_server_url,
                connect_timeout.unwrap_or(DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT),
            ),
            self.local_runtime_paths.clone(),
        ).with_http_client_factory(self.http_client_factory.clone()));
        environment.start_connecting();
        self.environments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(environment_id, environment);
        Ok(())
    }

    /// Adds or replaces a named remote environment that connects through an
    /// authenticated, end-to-end encrypted rendezvous stream.
    ///
    /// The provider is retained so every reconnect obtains fresh authorization.
    /// This transport never falls back to the URL-only remote environment path.
    pub fn upsert_noise_environment(
        &self,
        environment_id: String,
        provider: Arc<dyn NoiseRendezvousConnectProvider>,
    ) -> Result<(), ExecServerError> {
        if environment_id == LOCAL_ENVIRONMENT_ID {
            return Err(ExecServerError::Protocol(format!(
                "environment id `{LOCAL_ENVIRONMENT_ID}` is reserved for EnvironmentManager"
            )));
        }
        if environment_id.is_empty() {
            return Err(ExecServerError::Protocol(
                "environment id cannot be empty".to_string(),
            ));
        }
        let identity = NoiseChannelIdentity::generate().map_err(|error| {
            ExecServerError::Protocol(format!(
                "failed to generate Noise harness identity: {error}"
            ))
        })?;
        let environment = Arc::new(Environment::remote_with_transport(
            ExecServerTransportParams::NoiseRendezvous { provider, identity },
            self.local_runtime_paths.clone(),
        ).with_http_client_factory(self.http_client_factory.clone()));
        environment.start_connecting();
        self.environments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(environment_id, environment);
        Ok(())
    }
}

fn default_http_client_factory() -> codex_http_client::HttpClientFactory {
    codex_http_client::HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault)
}

fn noise_environment_config_from_env(http_client_factory: &codex_http_client::HttpClientFactory)
-> Result<Option<NoiseRendezvousEnvironmentConfig>, ExecServerError> {
    noise_environment_config_from_values(
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID_ENV_VAR),
        http_client_factory,
    )
}

fn noise_environment_config_from_values(
    registry_url: Option<String>,
    environment_id: Option<String>,
    auth_token: Option<String>,
    chatgpt_account_id: Option<String>,
    http_client_factory: &codex_http_client::HttpClientFactory,
) -> Result<Option<NoiseRendezvousEnvironmentConfig>, ExecServerError> {
    let (registry_url, environment_id, auth_token) =
        match (registry_url, environment_id, auth_token) {
            (None, None, None) => return Ok(None),
            (Some(registry_url), Some(environment_id), Some(auth_token)) => {
                (registry_url, environment_id, auth_token)
            }
            _ => {
                return Err(ExecServerError::EnvironmentRegistryConfig(format!(
                    "Noise environment requires {CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR}, \
{CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR}, and \
{CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR}"
                )));
            }
        };

    let config = NoiseRendezvousEnvironmentConfig::new(
        registry_url,
        environment_id,
        auth_token,
        chatgpt_account_id,
        http_client_factory.clone(),
    )?;
    Ok(Some(config))
}

fn optional_environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Concrete execution/filesystem environment selected for a session.
///
/// This bundles the selected backend metadata together with the local runtime
/// paths used by filesystem helpers.
#[derive(Clone)]
pub struct Environment {
    approval_scope_id: String,
    exec_server_url: Option<String>,
    remote_client: Option<LazyRemoteExecServerClient>,
    // Dropping the environment stops unfinished background startup work.
    startup_task: Arc<Mutex<Option<AbortOnDropHandle<()>>>>,
    exec_backend: Arc<dyn ExecBackend>,
    filesystem: Arc<dyn ExecutorFileSystem>,
    http_client: Arc<dyn HttpClient>,
    local_runtime_paths: Option<ExecServerRuntimePaths>,
}


impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("exec_server_url", &self.exec_server_url)
            .finish_non_exhaustive()
    }
}

/// Cached initialized-session metadata and the process owner it describes.
#[derive(Clone)]
pub struct EnvironmentRuntime {
    pub session_id: String,
    pub incarnation: String,
    pub info: EnvironmentInfo,
    pub exec_backend: Arc<dyn ExecBackend>,
    client: Option<crate::client::ExecServerClient>,
}


impl Environment {
    // Called only while admitting a provider snapshot or a new environment, before startup.
    fn with_http_client_factory(mut self, factory: codex_http_client::HttpClientFactory) -> Self {
        if let Some(client) = self.remote_client.take() {
            let client = client.with_http_client_factory(factory);
            self.exec_backend = Arc::new(RemoteProcess::new(client.clone()));
            self.filesystem = Arc::new(RemoteFileSystem::new(client.clone()));
            self.http_client = Arc::new(client.clone());
            self.remote_client = Some(client);
        }
        self
    }

    fn runtime_for_client(client: crate::client::ExecServerClient) -> Option<EnvironmentRuntime> {
        Some(EnvironmentRuntime {
            session_id: client.session_id()?,
            incarnation: client.instance_id().to_string(),
            info: client.cached_environment_info()?,
            exec_backend: Arc::new(crate::remote_process::RemoteProcess::bound(client.clone())),
            client: Some(client),
        })
    }

    pub fn cached_runtime(&self) -> Option<EnvironmentRuntime> {
        Self::runtime_for_client(self.remote_client.as_ref()?.cached_client()?)
    }

    /// Freeze process, filesystem and HTTP authority to one initialized session.
    pub fn bound_to_runtime(&self, runtime: &EnvironmentRuntime) -> Self {
        let Some(client) = &self.remote_client else {
            return self.clone();
        };
        let Some(runtime_client) = &runtime.client else {
            return self.clone();
        };
        let client = client.bound(runtime_client.clone());
        Self {
            remote_client: Some(client.clone()),
            exec_backend: Arc::clone(&runtime.exec_backend),
            filesystem: Arc::new(RemoteFileSystem::new(client.clone())),
            http_client: Arc::new(client),
            ..self.clone()
        }
    }

    /// Uses this environment's filesystem and process environment without launching anything.
    pub async fn lookup_executable(
        &self,
        mut params: crate::LookupExecutableParams,
    ) -> Result<crate::LookupExecutableResponse, ExecServerError> {
        if let Some(client) = &self.remote_client {
            params.sandbox = params.sandbox.map(crate::FileSystemSandboxContext::drop_cwd_if_unused);
            return client.get().await?.lookup_executable(params).await;
        }
        crate::executable_lookup::lookup_executable(self.filesystem.as_ref(), &params)
            .await
            .map_err(|error| ExecServerError::Protocol(error.to_string()))
    }

    pub async fn current_runtime(&self) -> Result<Option<EnvironmentRuntime>, ExecServerError> {
        match &self.remote_client {
            Some(client) => {
                let client = client.get().await?;
                Self::runtime_for_client(client).map(Some).ok_or_else(|| {
                    ExecServerError::Protocol(
                        "initialized remote runtime metadata is unavailable".into(),
                    )
                })
            }
            None => Ok(None),
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.readiness_result(), Some(Ok(())))
    }

    /// Builds an environment from the raw `CODEX_EXEC_SERVER_URL` value.
    pub fn create(
        exec_server_url: Option<String>,
        local_runtime_paths: ExecServerRuntimePaths,
    ) -> Result<Self, ExecServerError> {
        Self::create_inner(exec_server_url, local_runtime_paths)
    }


    /// Builds an environment from the raw `CODEX_EXEC_SERVER_URL` value and
    /// local runtime paths used when creating local filesystem helpers.
    fn create_inner(
        exec_server_url: Option<String>,
        local_runtime_paths: ExecServerRuntimePaths,
    ) -> Result<Self, ExecServerError> {
        let (exec_server_url, disabled) = normalize_exec_server_url(exec_server_url);
        if disabled {
            return Err(ExecServerError::Protocol(
                "disabled mode does not create an Environment".to_string(),
            ));
        }

        Ok(match exec_server_url {
            Some(exec_server_url) => Self::remote_inner(exec_server_url, Some(local_runtime_paths)),
            None => Self::local(local_runtime_paths),
        })
    }

    pub(crate) fn local(local_runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            approval_scope_id: Uuid::new_v4().to_string(),
            exec_server_url: None,
            remote_client: None,
            startup_task: Arc::new(Mutex::new(None)),
            exec_backend: Arc::new(LocalProcess::with_local_runtime_paths(
                local_runtime_paths.clone(),
            )),
            filesystem: Arc::new(LocalFileSystem::with_runtime_paths(
                local_runtime_paths.clone(),
            )),
            http_client: Arc::new(ReqwestHttpClient),
            local_runtime_paths: Some(local_runtime_paths),
        }
    }

    pub(crate) fn remote_inner(
        exec_server_url: String,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Self {
        Self::remote_with_transport(
            ExecServerTransportParams::websocket_url(
                exec_server_url,
                DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
            ),
            local_runtime_paths,
        )
    }

    pub(crate) fn remote_with_transport(
        remote_transport: ExecServerTransportParams,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Self {
        let exec_server_url = match &remote_transport {
            ExecServerTransportParams::WebSocketUrl {
                websocket_url: exec_server_url,
                ..
            } => Some(exec_server_url.clone()),
            ExecServerTransportParams::NoiseRendezvous { .. } => None,
            ExecServerTransportParams::StdioCommand { .. } => None,
        };
        let client = LazyRemoteExecServerClient::new(remote_transport);
        let exec_backend: Arc<dyn ExecBackend> = Arc::new(RemoteProcess::new(client.clone()));
        let filesystem: Arc<dyn ExecutorFileSystem> =
            Arc::new(RemoteFileSystem::new(client.clone()));

        Self {
            approval_scope_id: Uuid::new_v4().to_string(),
            exec_server_url,
            remote_client: Some(client.clone()),
            startup_task: Arc::new(Mutex::new(None)),
            exec_backend,
            filesystem,
            http_client: Arc::new(client),
            local_runtime_paths,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.remote_client.is_some()
    }

    /// Returns the opaque identity used to scope reusable approvals to this
    /// concrete environment instance.
    pub fn approval_scope_id(&self) -> &str {
        &self.approval_scope_id
    }

    /// Coordination identity, independent of permission/approval instances.
    /// Reconnecting to a different server incarnation changes this identity.
    pub fn coordination_resource_id(&self) -> String {
        if !self.is_remote() {
            return "local".to_string();
        }
        if let Some(id) = self.remote_client.as_ref()
            .and_then(LazyRemoteExecServerClient::cached_client)
            .and_then(|client| client.cached_coordination_resource_id())
        {
            return format!("exec-server:{id}");
        }
        // Legacy servers still coordinate aliases of the same configured URL.
        // Unknown transports stay instance-qualified rather than merging hosts.
        self.exec_server_url.as_ref().map_or_else(
            || format!("unknown:{}", self.approval_scope_id),
            |url| format!("exec-url:{url}"),
        )
    }

    /// Returns the remote exec-server URL when this environment is remote.
    pub fn exec_server_url(&self) -> Option<&str> {
        self.exec_server_url.as_deref()
    }

    pub fn local_runtime_paths(&self) -> Option<&ExecServerRuntimePaths> {
        self.local_runtime_paths.as_ref()
    }

    /// Returns environment information from the selected execution/filesystem environment.
    pub async fn info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        match &self.remote_client {
            Some(client) => client.environment_info().await,
            None => tokio::task::spawn_blocking(EnvironmentInfo::local)
                .await
                .map_err(|error| {
                    ExecServerError::Protocol(format!(
                        "local environment discovery task failed: {error}"
                    ))
                }),
        }
    }

    /// Starts connecting a remote environment without waiting for it.
    /// Requires an active Tokio runtime when background startup is supported.
    pub fn start_connecting(&self) {
        let Some(client) = &self.remote_client else {
            return;
        };
        let mut startup_task = self
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if startup_task.is_none() {
            *startup_task = client.start_connecting();
        }
    }

    /// Starts the initial connection after an environment is actually selected for use.
    pub(crate) fn start_connecting_for_use(environment: &Arc<Self>) {
        let Some(client) = &environment.remote_client else {
            return;
        };
        let mut startup_task = environment
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if startup_task.is_none() {
            let client = client.clone();
            *startup_task = Some(AbortOnDropHandle::new(tokio::spawn(async move {
                if let Err(error) = client.wait_until_ready().await {
                    tracing::debug!(%error, "exec-server environment startup failed");
                }
            })));
        }
    }

    /// Returns whether the initial startup attempt has completed.
    pub fn startup_finished(&self) -> bool {
        self.remote_client
            .as_ref()
            .is_none_or(LazyRemoteExecServerClient::startup_finished)
    }

    /// Waits for initial startup, retrying a previous transient failure when possible.
    pub async fn wait_until_ready(&self) -> Result<(), ExecServerError> {
        match &self.remote_client {
            Some(client) => client.wait_until_ready().await,
            None => Ok(()),
        }
    }

    /// Returns whether the environment can serve a request without waiting or reconnecting.
    pub(crate) fn readiness_result(&self) -> Option<Result<(), ExecServerError>> {
        match &self.remote_client {
            Some(client) => client.readiness_result(),
            None => Some(Ok(())),
        }
    }

    pub fn get_exec_backend(&self) -> Arc<dyn ExecBackend> {
        Arc::clone(&self.exec_backend)
    }

    pub fn get_http_client(&self) -> Arc<dyn HttpClient> {
        Arc::clone(&self.http_client)
    }

    pub fn get_filesystem(&self) -> Arc<dyn ExecutorFileSystem> {
        Arc::clone(&self.filesystem)
    }

    /// Returns a filesystem view that fails instead of starting or waiting for a connection.
    pub fn get_filesystem_without_reconnect(&self) -> Arc<dyn ExecutorFileSystem> {
        match &self.remote_client {
            Some(client) => Arc::new(RemoteFileSystem::new(client.fail_fast())),
            None => Arc::clone(&self.filesystem),
        }
    }
}

