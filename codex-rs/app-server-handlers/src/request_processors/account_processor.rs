use super::Account;
use super::AccountLoginCompletedNotification;
use super::AccountTokenUsageDailyBucket;
use super::AccountTokenUsageSummary;
use super::AccountUpdatedNotification;
use super::AddCreditsNudgeCreditType;
use super::AddCreditsNudgeEmailStatus;
use super::Arc;
use super::AuthManager;
use super::AuthMode;
use super::BackendAddCreditsNudgeCreditType;
use super::BackendClient;
use super::BackendRequestError;
use super::BackendWorkspaceMessage;
use super::BackendWorkspaceMessageType;
use super::BackendWorkspaceMessagesResponse;
use super::CODEX_OPEN_APP_URL;
use super::CancelLoginAccountParams;
use super::CancelLoginAccountResponse;
use super::CancelLoginAccountStatus;
use super::CancellationToken;
use super::ClientResponsePayload;
use super::CodexAuth;
use super::Config;
use super::ConfigManager;
use super::ConnectionRequestId;
use super::Duration;
use super::ForcedLoginMethod;
use super::GetAccountParams;
use super::GetAccountRateLimitsResponse;
use super::GetAccountResponse;
use super::GetAccountTokenUsageResponse;
use super::GetAuthStatusParams;
use super::GetAuthStatusResponse;
use super::GetWorkspaceMessagesResponse;
use super::HashMap;
use super::IoError;
use super::JSONRPCErrorError;
use super::LoginAccountParams;
use super::LoginAccountResponse;
use super::LoginApiKeyParams;
use super::LoginAppBrand;
use super::LoginServerOptions;
use super::LoginSuccessPage;
use super::LoginSuccessPageBrand;
use super::LogoutAccountResponse;
use super::Mutex;
use super::OutgoingMessageSender;
use super::RateLimitResetCreditsSummary;
use super::Result;
use super::SendAddCreditsNudgeEmailParams;
use super::SendAddCreditsNudgeEmailResponse;
use super::ServerNotification;
use super::ShutdownHandle;
use super::ThreadManager;
use super::TokenUsageProfile;
use super::Uuid;
use super::WorkspaceMessage;
use super::WorkspaceMessageType;
use super::create_model_provider;
use super::internal_error;
use super::invalid_request;
use super::login_with_api_key;
use super::oauth_client_id;
use super::oneshot;
use super::prepare_device_code_login;
use super::request_device_code;
use super::run_login_server_async;
use super::warn;
use crate::external_auth::ExternalAuthBridge;
use chrono::DateTime;

mod rate_limit_resets;
mod workspace_routing;

// Duration before a browser ChatGPT login attempt is abandoned.
const LOGIN_CHATGPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const ACCOUNT_TOKEN_USAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);
const ACCOUNT_BACKEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);
const ACCOUNT_WORKSPACE_MESSAGES_FETCH_TIMEOUT: Duration =
    Duration::from_millis(/*millis*/ 1000);
// Login overrides are intentionally available only in debug builds.
#[cfg(debug_assertions)]
const LOGIN_ISSUER_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_ISSUER";
#[cfg(debug_assertions)]
const LOGIN_OPEN_APP_URL_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_DEV_OPEN_APP_URL";


enum ActiveLogin {
    Browser {
        shutdown_handle: ShutdownHandle,
        commit_settled: tokio::sync::watch::Receiver<bool>,
        login_id: Uuid,
    },
    DeviceCode {
        cancel: CancellationToken,
        login_id: Uuid,
        commit_admitted: bool,
    },
}

impl ActiveLogin {
    fn login_id(&self) -> Uuid {
        match self {
            ActiveLogin::Browser { login_id, .. } | ActiveLogin::DeviceCode { login_id, .. } => {
                *login_id
            }
        }
    }

    fn cancel(&self) {
        match self {
            ActiveLogin::Browser {
                shutdown_handle, ..
            } => shutdown_handle.shutdown(),
            ActiveLogin::DeviceCode { cancel, .. } => cancel.cancel(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum CancelLoginError {
    NotFound,
}

enum RefreshTokenRequestOutcome {
    NotAttemptedOrSucceeded,
    FailedTransiently,
    FailedPermanently,
}

impl RefreshTokenRequestOutcome {
    fn into_request_result(self) -> Result<(), JSONRPCErrorError> {
        match self {
            Self::NotAttemptedOrSucceeded | Self::FailedPermanently => Ok(()),
            Self::FailedTransiently => Err(internal_error(
                "failed to refresh account token; retry the request",
            )),
        }
    }
}

impl Drop for ActiveLogin {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct AccountRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    config_manager: ConfigManager,
    backend_client: Arc<std::sync::Mutex<(String, codex_http_client::HttpClientFactory, BackendClient)>>,
    active_login: Arc<Mutex<Option<ActiveLogin>>>,
}

impl AccountRequestProcessor {
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        config: Arc<Config>,
        config_manager: ConfigManager,
    ) -> Self {
        let backend_client = BackendClient::new(
            config.chatgpt_base_url.clone(),
            config.http_client_factory(),
        );
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            config_manager,
            backend_client: Arc::new(std::sync::Mutex::new((
                config.chatgpt_base_url.clone(),
                config.http_client_factory(),
                backend_client,
            ))),
            active_login: Arc::new(Mutex::new(None)),
        }
    }


    async fn current_config(&self) -> Result<Config, JSONRPCErrorError> {
        let generation = self.config_manager.policy_generation();
        let config = self.config_manager
            .load_latest_config(/*fallback_cwd*/ None)
            .await
            .map_err(|err| internal_error(format!("failed to load current account configuration: {err}")))?;
        let mut guard = self.active_login.lock().await;
        self.config_manager.validate_policy_generation(generation)
            .map_err(|err| internal_error(format!("account configuration changed: {err}")))?;
        self.auth_manager.check_credential_store_config(&config)
            .map_err(|err| internal_error(err.to_string()))?;
        if self.auth_manager.set_login_restrictions(
            config.forced_login_method,
            config.forced_chatgpt_workspace_id.clone(),
        ).map_err(|err| internal_error(format!("failed to update account login policy: {err}")))? {
            Self::cancel_active_login_locked(&mut guard).await;
            self.auth_manager.reload().await;
            self.config_manager.complete_auth_transition(
                self.auth_manager.clone(),
                config.chatgpt_base_url.clone(),
                config.http_client_factory(),
            ).await;
        }
        Ok(config)
    }

    fn backend_client_for_auth(&self, auth: &CodexAuth, config: &Config) -> BackendClient {
        let factory = config.http_client_factory();
        let mut cached = self.backend_client.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.0 != config.chatgpt_base_url || cached.1 != factory {
            *cached = (
                config.chatgpt_base_url.clone(),
                factory.clone(),
                BackendClient::new(config.chatgpt_base_url.clone(), factory),
            );
        }
        cached.2.clone().with_auth(auth)
    }

    fn account_read_epoch(&self, auth: Option<&CodexAuth>) -> Result<u64, JSONRPCErrorError> {
        let (epoch, current) = self.auth_manager.policy_auth_snapshot().map_err(|error| internal_error(error.to_string()))?;
        if auth.map_or(current.is_some(), |auth| self.auth_manager.auth_revision_for_snapshot(auth).is_none()) {
            return Err(invalid_request("authentication changed; retry the account request"));
        }
        Ok(epoch)
    }

    fn finish_account_read<T>(&self, epoch: u64, response: T) -> Result<T, JSONRPCErrorError> {
        self.auth_manager.with_policy_auth_epoch(epoch, || response)
            .map_err(|_| invalid_request("authentication changed; retry the account request"))
    }

    pub(crate) async fn login_account(
        &self,
        request_id: ConnectionRequestId,
        params: LoginAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.login_v2(request_id, params).await.map(|()| None)
    }

    pub(crate) async fn logout_account(
        &self,
        request_id: ConnectionRequestId,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.logout_v2(request_id).await.map(|()| None)
    }

    pub(crate) async fn cancel_login_account(
        &self,
        params: CancelLoginAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.cancel_login_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_account(
        &self,
        params: GetAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_account_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_auth_status(
        &self,
        params: GetAuthStatusParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_auth_status_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_account_rate_limits(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_account_rate_limits_response()
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_account_token_usage(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_account_token_usage_response()
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_workspace_messages(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_workspace_messages_response()
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn send_add_credits_nudge_email(
        &self,
        params: SendAddCreditsNudgeEmailParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.send_add_credits_nudge_email_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    async fn cancel_active_login_locked(active_login: &mut Option<ActiveLogin>) {
        if let Some(ActiveLogin::Browser { shutdown_handle, commit_settled, .. }) = active_login.as_mut()
            && !shutdown_handle.try_cancel()
        {
            // An admitted credential commit cannot be cancelled. Its completion
            // signal is independent of this registry lock and response delivery.
            let _ = commit_settled.wait_for(|settled| *settled).await;
        }
        if let Some(active_login) = active_login.take() {
            drop(active_login);
        }
    }

    pub(crate) async fn cancel_active_login(&self) {
        let mut guard = self.active_login.lock().await;
        Self::cancel_active_login_locked(&mut guard).await;
    }

    pub(crate) fn clear_external_auth(&self) {
        self.auth_manager.clear_external_auth();
        let plugins_manager = self.thread_manager.plugins_manager();
        plugins_manager.set_auth_mode(self.auth_manager.get_api_auth_mode());
        if plugins_manager.invalidate_account_scoped_remote_plugin_caches() {
            Self::spawn_effective_plugins_changed_task(
                Arc::clone(&self.thread_manager),
                self.config_manager.clone(),
            );
        }
    }

    fn current_account_updated_notification(&self) -> AccountUpdatedNotification {
        let auth = self.auth_manager.auth_cached();
        AccountUpdatedNotification {
            auth_mode: auth
                .as_ref()
                .map(CodexAuth::api_auth_mode)
                .map(AuthMode::from),
            plan_type: auth.as_ref().and_then(CodexAuth::account_plan_type),
        }
    }

    async fn maybe_refresh_plugin_caches_for_current_config(
        config_manager: &ConfigManager,
        thread_manager: &Arc<ThreadManager>,
        auth: Option<CodexAuth>,
    ) {
        let plugins_manager = thread_manager.plugins_manager();
        plugins_manager.set_auth_mode(auth.as_ref().map(CodexAuth::api_auth_mode));
        if plugins_manager.invalidate_account_scoped_remote_plugin_caches() {
            Self::spawn_effective_plugins_changed_task(
                Arc::clone(thread_manager),
                config_manager.clone(),
            );
        }

        match config_manager
            .load_latest_config(/*fallback_cwd*/ None)
            .await
        {
            Ok(config) => {
                let refresh_thread_manager = Arc::clone(thread_manager);
                let refresh_config_manager = config_manager.clone();
                plugins_manager.maybe_start_remote_plugin_caches_refresh(
                    &config.plugins_config_input(),
                    auth,
                    Some(Arc::new(move || {
                        Self::spawn_effective_plugins_changed_task(
                            Arc::clone(&refresh_thread_manager),
                            refresh_config_manager.clone(),
                        );
                    })),
                );
            }
            Err(err) => {
                warn!(
                    "failed to reload config after account changed, skipping remote installed plugins cache refresh: {err}"
                );
            }
        }
    }

    fn spawn_effective_plugins_changed_task(
        thread_manager: Arc<ThreadManager>,
        config_manager: ConfigManager,
    ) {
        tokio::spawn(async move {
            thread_manager.plugins_manager().clear_cache();
            thread_manager.skills_service().clear_cache();
            if thread_manager.list_thread_ids().await.is_empty() {
                return;
            }
            crate::mcp_refresh::queue_best_effort_refresh(&thread_manager, &config_manager).await;
        });
    }

    async fn login_v2(
        &self,
        request_id: ConnectionRequestId,
        params: LoginAccountParams,
    ) -> Result<(), JSONRPCErrorError> {
        match params {
            LoginAccountParams::ApiKey { api_key } => {
                self.login_api_key_v2(request_id, LoginApiKeyParams { api_key })
                    .await;
            }
            LoginAccountParams::Chatgpt {
                app_brand,
                codex_streamlined_login,
                use_hosted_login_success_page,
            } => {
                let login_success_page = if use_hosted_login_success_page {
                    let app_brand = match app_brand.unwrap_or_default() {
                        LoginAppBrand::Codex => LoginSuccessPageBrand::Codex,
                        LoginAppBrand::Chatgpt => LoginSuccessPageBrand::Chatgpt,
                    };
                    LoginSuccessPage::Hosted {
                        url: CODEX_OPEN_APP_URL.parse().map_err(|err| {
                            internal_error(format!("invalid Codex open app URL: {err}"))
                        })?,
                        app_brand,
                    }
                } else {
                    LoginSuccessPage::default()
                };
                self.login_chatgpt_v2(request_id, codex_streamlined_login, login_success_page)
                    .await;
            }
            LoginAccountParams::ChatgptDeviceCode => {
                self.login_chatgpt_device_code_v2(request_id).await;
            }
            LoginAccountParams::ChatgptAuthTokens {
                access_token,
                chatgpt_account_id,
                chatgpt_plan_type,
            } => {
                self.login_chatgpt_auth_tokens(
                    request_id,
                    access_token,
                    chatgpt_account_id,
                    chatgpt_plan_type,
                )
                .await;
            }
        }
        Ok(())
    }

    fn external_auth_active_error(&self) -> JSONRPCErrorError {
        invalid_request(
            "External auth is active. Use account/login/start (chatgptAuthTokens) to update it or account/logout to clear it.",
        )
    }

    async fn login_api_key_common(
        &self,
        params: &LoginApiKeyParams,
    ) -> std::result::Result<(), JSONRPCErrorError> {
        let config = self.current_config().await?;
        let mut guard = Arc::clone(&self.active_login).lock_owned().await;
        if self.auth_manager.is_external_chatgpt_auth_active() {
            return Err(self.external_auth_active_error());
        }

        if !self.auth_manager.is_login_method_allowed(ForcedLoginMethod::Api) {
            return Err(invalid_request(
                "API key login is disabled. Use ChatGPT login instead.",
            ));
        }
        Self::cancel_active_login_locked(&mut guard).await;

        let processor = self.clone();
        let api_key = params.api_key.clone();
        // The detached owner retains the transition lock if the caller is
        // cancelled. A blocking credential writer must settle before logout or
        // another login can acquire this lock.
        tokio::spawn(async move {
            let home = config.codex_home.clone();
            let store_mode = config.cli_auth_credentials_store_mode;
            let keyring_backend = config.auth_keyring_backend_kind();
            tokio::task::spawn_blocking(move || {
                login_with_api_key(&home, &api_key, store_mode, keyring_backend)
            })
            .await
            .map_err(|err| internal_error(format!("api key persistence task failed: {err}")))?
            .map_err(|err| internal_error(format!("failed to save api key: {err}")))?;
            processor.auth_manager.reload().await;
            processor.config_manager
                .complete_auth_transition(
                    processor.auth_manager.clone(),
                    config.chatgpt_base_url.clone(),
                    config.http_client_factory(),
                )
                .await;
            drop(guard);
            Ok(())
        })
        .await
        .map_err(|err| internal_error(format!("api key login task failed: {err}")))?
    }

    async fn login_api_key_v2(&self, request_id: ConnectionRequestId, params: LoginApiKeyParams) {
        let result = self
            .login_api_key_common(&params)
            .await
            .map(|()| LoginAccountResponse::ApiKey {});
        let logged_in = result.is_ok();
        self.outgoing.send_result(request_id, result).await;

        if logged_in {
            self.send_login_success_notifications(/*login_id*/ None)
                .await;
        }
    }

    // Build options for a ChatGPT login attempt; performs validation.
    fn login_chatgpt_common(
        &self,
        config: &Config,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
    ) -> std::result::Result<LoginServerOptions, JSONRPCErrorError> {
        if self.auth_manager.is_external_chatgpt_auth_active() {
            return Err(self.external_auth_active_error());
        }

        if !self.auth_manager.is_login_method_allowed(ForcedLoginMethod::Chatgpt) {
            return Err(invalid_request(
                "ChatGPT login is disabled. Use API key login instead.",
            ));
        }

        let opts = LoginServerOptions {
            open_browser: false,
            codex_streamlined_login,
            login_success_page,
            ..LoginServerOptions::new(
                config.codex_home.to_path_buf(),
                oauth_client_id(),
                self.auth_manager.effective_chatgpt_workspaces(),
                config.cli_auth_credentials_store_mode,
                config.auth_keyring_backend_kind(),
                config.auth_route_config(),
            )
        };
        #[cfg(debug_assertions)]
        let opts = {
            let mut opts = opts;
            if let Ok(issuer) = std::env::var(LOGIN_ISSUER_OVERRIDE_ENV_VAR)
                && !issuer.trim().is_empty()
            {
                opts.issuer = issuer;
            }
            if let LoginSuccessPage::Hosted { url, .. } = &mut opts.login_success_page
                && let Ok(open_app_url) = std::env::var(LOGIN_OPEN_APP_URL_OVERRIDE_ENV_VAR)
                && !open_app_url.trim().is_empty()
            {
                *url = open_app_url
                    .parse()
                    .map_err(|err| internal_error(format!("invalid Codex open app URL: {err}")))?;
            }
            opts
        };

        Ok(opts)
    }

    fn login_chatgpt_device_code_start_error(err: IoError) -> JSONRPCErrorError {
        let is_not_found = err.kind() == std::io::ErrorKind::NotFound;
        if is_not_found {
            invalid_request(err.to_string())
        } else {
            internal_error(format!("failed to request device code: {err}"))
        }
    }

    async fn login_chatgpt_v2(
        &self,
        request_id: ConnectionRequestId,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
    ) {
        let (response_enqueued_tx, response_enqueued_rx) = oneshot::channel();
        let result = self
            .login_chatgpt_response(
                codex_streamlined_login,
                login_success_page,
                response_enqueued_rx,
            )
            .await;
        self.outgoing.send_result(request_id, result).await;
        let _ = response_enqueued_tx.send(());
    }

    async fn login_chatgpt_response(
        &self,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
        response_enqueued: oneshot::Receiver<()>,
    ) -> Result<LoginAccountResponse, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let mut guard = self.active_login.lock().await;
        Self::cancel_active_login_locked(&mut guard).await;
        let opts =
            self.login_chatgpt_common(&config, codex_streamlined_login, login_success_page)?;
        // Register immediately after startup, before yielding to another
        // transition; no admitted browser writer may be left unaccounted for.
        let server = run_login_server_async(opts)
            .await
            .map_err(|err| internal_error(format!("failed to start login server: {err}")))?;
        let login_id = Uuid::new_v4();
        let shutdown_handle = server.cancel_handle();
        let (commit_settled_tx, commit_settled) = tokio::sync::watch::channel(false);
        *guard = Some(ActiveLogin::Browser {
            shutdown_handle: shutdown_handle.clone(),
            commit_settled,
            login_id,
        });
        drop(guard);

        let outgoing_clone = self.outgoing.clone();
        let config_manager = self.config_manager.clone();
        let thread_manager = Arc::clone(&self.thread_manager);
        let chatgpt_base_url = config.chatgpt_base_url.clone();
        let http_client_factory = config.http_client_factory();
        let active_login = self.active_login.clone();
        let auth_url = server.auth_url.clone();
        tokio::spawn(async move {
            // Observe commit completion even when response delivery is stalled:
            // transitions can wait on this task while holding the registry.
            let completion = server.block_until_done_with_callback_result();
            tokio::pin!(completion);
            let (success, error_msg) =
                match tokio::time::timeout(LOGIN_CHATGPT_TIMEOUT, &mut completion).await {
                    Ok(Ok(_)) => (true, None),
                    Ok(Err(err)) => (false, Some(format!("Login server error: {err}"))),
                    Err(_elapsed) => {
                        if shutdown_handle.try_cancel() {
                            (false, Some("Login timed out".to_string()))
                        } else {
                            match completion.await {
                                Ok(_) => (true, None),
                                Err(err) => (false, Some(format!("Login server error: {err}"))),
                            }
                        }
                    }
                };
            commit_settled_tx.send_replace(true);
            let _ = response_enqueued.await;

            Self::send_chatgpt_login_completion_notifications(
                &outgoing_clone,
                active_login.clone(),
                config_manager,
                thread_manager,
                chatgpt_base_url,
                http_client_factory,
                login_id,
                success,
                error_msg,
            )
            .await;

            // Clear the active login if it matches this attempt. It may have been replaced or cancelled.
            let mut guard = active_login.lock().await;
            if guard.as_ref().map(ActiveLogin::login_id) == Some(login_id) {
                *guard = None;
            }
        });

        Ok(LoginAccountResponse::Chatgpt {
            login_id: login_id.to_string(),
            auth_url,
        })
    }

    async fn login_chatgpt_device_code_v2(&self, request_id: ConnectionRequestId) {
        let (response_enqueued_tx, response_enqueued_rx) = oneshot::channel();
        let result = self
            .login_chatgpt_device_code_response(response_enqueued_rx)
            .await;
        self.outgoing.send_result(request_id, result).await;
        let _ = response_enqueued_tx.send(());
    }

    async fn login_chatgpt_device_code_response(
        &self,
        response_enqueued: oneshot::Receiver<()>,
    ) -> Result<LoginAccountResponse, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let opts = self
            .login_chatgpt_common(
                &config,
                /*codex_streamlined_login*/ false,
                LoginSuccessPage::default(),
            )?;
        let device_code = request_device_code(&opts)
            .await
            .map_err(Self::login_chatgpt_device_code_start_error)?;
        let login_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        {
            let mut guard = self.active_login.lock().await;
            if self.auth_manager.is_external_chatgpt_auth_active() {
                return Err(self.external_auth_active_error());
            }
            Self::cancel_active_login_locked(&mut guard).await;
            *guard = Some(ActiveLogin::DeviceCode {
                cancel: cancel.clone(),
                login_id,
                commit_admitted: false,
            });
        }

        let verification_url = device_code.verification_url.clone();
        let user_code = device_code.user_code.clone();

        let outgoing_clone = self.outgoing.clone();
        let config_manager = self.config_manager.clone();
        let thread_manager = Arc::clone(&self.thread_manager);
        let chatgpt_base_url = config.chatgpt_base_url.clone();
        let http_client_factory = config.http_client_factory();
        let active_login = self.active_login.clone();
        tokio::spawn(async move {
            let _ = response_enqueued.await;
            let result = async {
                let pending = tokio::select! {
                    _ = cancel.cancelled() => {
                        return Err(IoError::new(std::io::ErrorKind::Interrupted, "Login was not completed"));
                    }
                    result = prepare_device_code_login(opts, device_code) => result?,
                };
                // Keep the admitted writer in cancellation accounting until it
                // settles. Logout/replacement must join it before mutating auth.
                let mut guard = active_login.lock().await;
                if guard.as_ref().map(ActiveLogin::login_id) != Some(login_id) {
                    return Err(IoError::new(std::io::ErrorKind::Interrupted, "Login was not completed"));
                }
                if let Some(ActiveLogin::DeviceCode { commit_admitted, .. }) = guard.as_mut() {
                    *commit_admitted = true;
                }
                let result = pending.persist().await;
                drop(guard);
                result
            }.await;
            let (success, error_msg) = match result {
                Ok(()) => (true, None),
                Err(error) => (false, Some(error.to_string())),
            };

            Self::send_chatgpt_login_completion_notifications(
                &outgoing_clone,
                active_login.clone(),
                config_manager,
                thread_manager,
                chatgpt_base_url,
                http_client_factory,
                login_id,
                success,
                error_msg,
            )
            .await;

            let mut guard = active_login.lock().await;
            if guard.as_ref().map(ActiveLogin::login_id) == Some(login_id) {
                *guard = None;
            }
        });

        Ok(LoginAccountResponse::ChatgptDeviceCode {
            login_id: login_id.to_string(),
            verification_url,
            user_code,
        })
    }

    async fn cancel_login_chatgpt_common(
        &self,
        login_id: Uuid,
    ) -> std::result::Result<(), CancelLoginError> {
        let mut guard = self.active_login.lock().await;
        if guard.as_ref().map(ActiveLogin::login_id) == Some(login_id) {
            if matches!(guard.as_ref(), Some(ActiveLogin::DeviceCode { commit_admitted: true, .. })) {
                return Err(CancelLoginError::NotFound);
            }
            if let Some(ActiveLogin::Browser {
                shutdown_handle, ..
            }) = guard.as_ref()
                && !shutdown_handle.try_cancel()
            {
                return Err(CancelLoginError::NotFound);
            }
            if let Some(active) = guard.take() {
                drop(active);
            }
            Ok(())
        } else {
            Err(CancelLoginError::NotFound)
        }
    }

    async fn cancel_login_response(
        &self,
        params: CancelLoginAccountParams,
    ) -> Result<CancelLoginAccountResponse, JSONRPCErrorError> {
        let login_id = params.login_id;
        let uuid = Uuid::parse_str(&login_id)
            .map_err(|_| invalid_request(format!("invalid login id: {login_id}")))?;
        let status = match self.cancel_login_chatgpt_common(uuid).await {
            Ok(()) => CancelLoginAccountStatus::Canceled,
            Err(CancelLoginError::NotFound) => CancelLoginAccountStatus::NotFound,
        };
        Ok(CancelLoginAccountResponse { status })
    }

    async fn login_chatgpt_auth_tokens(
        &self,
        request_id: ConnectionRequestId,
        access_token: String,
        chatgpt_account_id: String,
        chatgpt_plan_type: Option<String>,
    ) {
        let result = self
            .login_chatgpt_auth_tokens_response(
                request_id.connection_id,
                access_token,
                chatgpt_account_id,
                chatgpt_plan_type,
            )
            .await;
        let logged_in = result.is_ok();
        self.outgoing.send_result(request_id, result).await;

        if logged_in {
            self.send_login_success_notifications(/*login_id*/ None)
                .await;
        }
    }

    async fn login_chatgpt_auth_tokens_response(
        &self,
        owner: crate::transport::ConnectionId,
        access_token: String,
        chatgpt_account_id: String,
        chatgpt_plan_type: Option<String>,
    ) -> Result<LoginAccountResponse, JSONRPCErrorError> {
        let config = self.current_config().await?;
        if !self.auth_manager.is_login_method_allowed(ForcedLoginMethod::Chatgpt) {
            return Err(invalid_request(
                "External ChatGPT auth is disabled. Use API key login instead.",
            ));
        }

        if let Some(expected_workspaces) = self.auth_manager.effective_chatgpt_workspaces()
            && !expected_workspaces.contains(&chatgpt_account_id)
        {
            return Err(invalid_request(format!(
                "External auth must use one of workspace(s) {expected_workspaces:?}, but received {chatgpt_account_id:?}.",
            )));
        }

        let auth = CodexAuth::from_external_chatgpt_tokens(
            &access_token,
            &chatgpt_account_id,
            chatgpt_plan_type.as_deref(),
        )
        .map_err(|err| internal_error(format!("failed to set external auth: {err}")))?;
        // Replace an active login only after the new credentials pass validation.
        let mut guard = Arc::clone(&self.active_login).lock_owned().await;
        Self::cancel_active_login_locked(&mut guard).await;
        let processor = self.clone();
        tokio::spawn(async move {
            processor.auth_manager
                .set_external_auth(Arc::new(ExternalAuthBridge::new(
                    Arc::clone(&processor.outgoing),
                    owner,
                    auth,
                )))
                .await
                .map_err(|err| internal_error(format!("failed to set external auth: {err}")))?;
            processor.config_manager
                .complete_auth_transition(
                    processor.auth_manager.clone(),
                    config.chatgpt_base_url.clone(),
                    config.http_client_factory(),
                )
                .await;
            drop(guard);
            Ok(LoginAccountResponse::ChatgptAuthTokens {})
        })
        .await
        .map_err(|err| internal_error(format!("external login task failed: {err}")))?
    }

    async fn send_login_success_notifications(&self, login_id: Option<Uuid>) {
        Self::maybe_refresh_plugin_caches_for_current_config(
            &self.config_manager,
            &self.thread_manager,
            self.auth_manager.auth_cached(),
        )
        .await;

        let payload_login_completed = AccountLoginCompletedNotification {
            login_id: login_id.map(|id| id.to_string()),
            success: true,
            error: None,
        };
        self.outgoing
            .send_server_notification(ServerNotification::AccountLoginCompleted(
                payload_login_completed,
            ))
            .await;

        self.send_current_account_updated().await;
    }

    async fn send_current_account_updated(&self) {
        let Some(permit) = self.outgoing.reserve_server_notification().await else { return; };
        let _guard = self.active_login.lock().await;
        permit.send(ServerNotification::AccountUpdated(self.current_account_updated_notification()));
    }

    // Login completion must carry both the transport dependencies and the
    // complete outcome payload into the detached notification task.
    #[allow(clippy::too_many_arguments)]
    async fn send_chatgpt_login_completion_notifications(
        outgoing: &OutgoingMessageSender,
        active_login: Arc<Mutex<Option<ActiveLogin>>>,
        config_manager: ConfigManager,
        thread_manager: Arc<ThreadManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
        login_id: Uuid,
        success: bool,
        error_msg: Option<String>,
    ) {
        let payload_v2 = AccountLoginCompletedNotification {
            login_id: Some(login_id.to_string()),
            success,
            error: error_msg,
        };

        let mut guard = active_login.lock().await;
        let is_current = guard.as_ref().map(ActiveLogin::login_id) == Some(login_id);
        let publish_account_updated = success && is_current;
        if publish_account_updated {
            let auth_manager = thread_manager.auth_manager();
            auth_manager.reload().await;
            config_manager
                .complete_auth_transition(
                    auth_manager.clone(),
                    chatgpt_base_url,
                    http_client_factory,
                )
                .await;

            let auth = auth_manager.auth_cached();
            Self::maybe_refresh_plugin_caches_for_current_config(
                &config_manager,
                &thread_manager,
                auth.clone(),
            )
            .await;
        }
        if is_current {
            guard.take();
        }
        drop(guard);
        outgoing
            .send_server_notification(ServerNotification::AccountLoginCompleted(payload_v2))
            .await;
        if publish_account_updated
            && let Some(permit) = outgoing.reserve_server_notification().await
        {
            let _guard = active_login.lock().await;
            let auth = thread_manager.auth_manager().auth_cached();
            permit.send(ServerNotification::AccountUpdated(AccountUpdatedNotification {
                auth_mode: auth.as_ref().map(CodexAuth::api_auth_mode).map(AuthMode::from),
                plan_type: auth.as_ref().and_then(CodexAuth::account_plan_type),
            }));
        }
    }

    async fn logout_common(&self) -> std::result::Result<Option<AuthMode>, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let mut guard = Arc::clone(&self.active_login).lock_owned().await;
        Self::cancel_active_login_locked(&mut guard).await;
        let processor = self.clone();
        tokio::spawn(async move {
            processor.auth_manager.logout_with_revoke().await
                .map_err(|err| internal_error(format!("logout failed: {err}")))?;
            processor.config_manager
                .complete_auth_transition(
                    processor.auth_manager.clone(),
                    config.chatgpt_base_url.clone(),
                    config.http_client_factory(),
                )
                .await;
            drop(guard);

            Self::maybe_refresh_plugin_caches_for_current_config(
                &processor.config_manager,
                &processor.thread_manager,
                processor.auth_manager.auth_cached(),
            )
            .await;

            Ok(processor.auth_manager.auth_cached().as_ref()
                .map(CodexAuth::api_auth_mode)
                .map(AuthMode::from))
        })
        .await
        .map_err(|err| internal_error(format!("logout task failed: {err}")))?
    }

    async fn logout_v2(&self, request_id: ConnectionRequestId) -> Result<(), JSONRPCErrorError> {
        let result = self.logout_common().await;
        let publish_account_updated = result.is_ok();
        self.outgoing
            .send_result(request_id, result.map(|_| LogoutAccountResponse {}))
            .await;

        if publish_account_updated {
            self.send_current_account_updated().await;
        }
        Ok(())
    }

    async fn refresh_token_if_requested(&self, do_refresh: bool) -> RefreshTokenRequestOutcome {
        if self.auth_manager.is_external_chatgpt_auth_active() {
            return RefreshTokenRequestOutcome::NotAttemptedOrSucceeded;
        }
        if do_refresh && let Err(err) = self.auth_manager.refresh_token().await {
            let failed_reason = err.failed_reason();
            if failed_reason.is_none() {
                tracing::warn!("failed to refresh token while getting account: {err}");
                return RefreshTokenRequestOutcome::FailedTransiently;
            }
            return RefreshTokenRequestOutcome::FailedPermanently;
        }
        RefreshTokenRequestOutcome::NotAttemptedOrSucceeded
    }

    async fn get_auth_status_response(
        &self,
        params: GetAuthStatusParams,
    ) -> Result<GetAuthStatusResponse, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let include_token = params.include_token.unwrap_or(false);
        let do_refresh = params.refresh_token.unwrap_or(false);

        self.refresh_token_if_requested(do_refresh)
            .await
            .into_request_result()?;

        // Determine whether auth is required based on the active model provider.
        // If a custom provider is configured with `requires_openai_auth == false`,
        // then no auth step is required; otherwise, default to requiring auth.
        let requires_openai_auth = config.model_provider.requires_openai_auth;

        let auth = if do_refresh || !requires_openai_auth {
            self.auth_manager.auth_cached()
        } else {
            self.auth_manager.auth().await
        };
        let auth_epoch = self.account_read_epoch(auth.as_ref())?;
        let response = if !requires_openai_auth {
            GetAuthStatusResponse {
                auth_method: None,
                auth_token: None,
                requires_openai_auth: Some(false),
            }
        } else {
            match auth {
                Some(auth) => {
                    let permanent_refresh_failure =
                        self.auth_manager.refresh_failure_for_auth(&auth).is_some();
                    let auth_mode = AuthMode::from(auth.api_auth_mode());
                    let (reported_auth_method, token_opt) = if matches!(
                        auth,
                        CodexAuth::Headers(_)
                            | CodexAuth::AgentIdentity(_)
                            | CodexAuth::PersonalAccessToken(_)
                    ) || include_token
                        && permanent_refresh_failure
                    {
                        // This response cannot represent the metadata needed to reuse these
                        // credentials.
                        (Some(auth_mode), None)
                    } else {
                        match auth.get_token() {
                            Ok(token) if !token.is_empty() => {
                                let tok = if include_token { Some(token) } else { None };
                                (Some(auth_mode), tok)
                            }
                            Ok(_) => (None, None),
                            Err(err) => {
                                tracing::warn!("failed to get token for auth status: {err}");
                                (None, None)
                            }
                        }
                    };
                    GetAuthStatusResponse {
                        auth_method: reported_auth_method,
                        auth_token: token_opt,
                        requires_openai_auth: Some(true),
                    }
                }
                None => GetAuthStatusResponse {
                    auth_method: None,
                    auth_token: None,
                    requires_openai_auth: Some(true),
                },
            }
        };

        self.finish_account_read(auth_epoch, response)
    }

    async fn get_account_response(
        &self,
        params: GetAccountParams,
    ) -> Result<GetAccountResponse, JSONRPCErrorError> {
        let do_refresh = params.refresh_token;

        self.refresh_token_if_requested(do_refresh)
            .await
            .into_request_result()?;

        let config = self.current_config().await?;
        let provider = create_model_provider(
            config.model_provider.clone(),
            Some(self.auth_manager.clone()),
        );
        let auth_epoch = self.account_read_epoch(self.auth_manager.auth_cached().as_ref())?;
        let account_state = match provider.account_state() {
            Ok(account_state) => account_state,
            Err(err) => return Err(invalid_request(err.to_string())),
        };
        let account = account_state.account.map(Account::from);
        let workspace_routing = if matches!(account, Some(Account::Chatgpt { .. })) {
            self.workspace_routing(&config).await
        } else {
            None
        };

        self.finish_account_read(auth_epoch, GetAccountResponse {
            account,
            requires_openai_auth: account_state.requires_openai_auth,
            workspace_routing,
        })
    }

    async fn get_account_rate_limits_response(
        &self,
    ) -> Result<GetAccountRateLimitsResponse, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to read rate limits",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to read rate limits",
            ));
        }

        let client = self.backend_client_for_auth(&auth, &config)
            .with_request_timeout(ACCOUNT_BACKEND_REQUEST_TIMEOUT);

        let auth_epoch = self.account_read_epoch(Some(&auth))?;

        let required = async {
            let response = client
                .get_rate_limits_with_reset_credits()
                .await
                .map_err(|err| {
                    internal_error(format!("failed to fetch codex rate limits: {err}"))
                })?;
            if response.rate_limits.is_empty() {
                return Err(internal_error(
                    "failed to fetch codex rate limits: no snapshots returned",
                ));
            }
            Ok(response)
        };
        // Details cannot salvage a failed required response. Dropping this branch
        // cancels its local request instead of waiting for the optional timeout.
        let optional = async { Ok(Self::detailed_rate_limit_reset_credits(&client).await) };
        let (response, detailed_rate_limit_reset_credits) = tokio::try_join!(required, optional)?;

        let first_limit_id = response.rate_limits[0].limit_id.clone().unwrap_or_else(|| "codex".to_string());
        let mut rate_limits_by_limit_id = HashMap::new();
        for mut snapshot in response.rate_limits {
            let explicit_default = snapshot.limit_id.as_deref() == Some("codex");
            let limit_id = snapshot.limit_id.clone().unwrap_or_else(|| "codex".to_string());
            snapshot.limit_id = Some(limit_id.clone());
            // Explicit default snapshots take precedence over legacy aliases.
            // Both wire fields are derived from this one canonical selection.
            if explicit_default {
                rate_limits_by_limit_id.insert(limit_id, snapshot);
            } else {
                rate_limits_by_limit_id.entry(limit_id).or_insert(snapshot);
            }
        }
        let rate_limits = rate_limits_by_limit_id.get("codex")
            .or_else(|| rate_limits_by_limit_id.get(&first_limit_id))
            .cloned().ok_or_else(|| internal_error("no canonical rate limit snapshot"))?;

        let rate_limit_reset_credits = detailed_rate_limit_reset_credits.or_else(|| {
            response
                .rate_limit_reset_credits
                .map(|summary| RateLimitResetCreditsSummary {
                    available_count: summary.available_count,
                    credits: None,
                })
        });

        self.finish_account_read(auth_epoch, GetAccountRateLimitsResponse {
            rate_limits: rate_limits.into(),
            rate_limits_by_limit_id: Some(
                rate_limits_by_limit_id
                    .into_iter()
                    .map(|(limit_id, snapshot)| (limit_id, snapshot.into()))
                    .collect(),
            ),
            rate_limit_reset_credits,
        })
    }

    async fn get_account_token_usage_response(
        &self,
    ) -> Result<GetAccountTokenUsageResponse, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to read token usage",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to read token usage",
            ));
        }

        let client = self.backend_client_for_auth(&auth, &config);
        let auth_epoch = self.account_read_epoch(Some(&auth))?;
        let profile = tokio::time::timeout(
            ACCOUNT_TOKEN_USAGE_FETCH_TIMEOUT,
            client.get_token_usage_profile(),
        )
        .await
        .map_err(|_| internal_error("token usage profile fetch timed out"))?
        .map_err(|err| internal_error(format!("failed to fetch token usage profile: {err}")))?;
        self.finish_account_read(auth_epoch, Self::account_token_usage_response(profile))
    }

    async fn get_workspace_messages_response(
        &self,
    ) -> Result<GetWorkspaceMessagesResponse, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to read workspace messages",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to read workspace messages",
            ));
        }

        let client = self.backend_client_for_auth(&auth, &config);
        let auth_epoch = self.account_read_epoch(Some(&auth))?;
        let messages = tokio::time::timeout(
            ACCOUNT_WORKSPACE_MESSAGES_FETCH_TIMEOUT,
            client.list_workspace_messages(),
        )
        .await
        .map_err(|_| internal_error("workspace messages fetch timed out"))?;

        self.finish_account_read(auth_epoch, match messages {
            Ok(messages) => {
                Self::workspace_messages_response(messages, /*feature_enabled*/ true)
            }
            Err(err) if workspace_messages_feature_disabled(&err) => {
                Self::workspace_messages_response(
                    BackendWorkspaceMessagesResponse {
                        messages: Vec::new(),
                    },
                    /*feature_enabled*/ false,
                )
            }
            Err(err) => Err(internal_error(format!(
                "failed to fetch workspace messages: {err}"
            ))),
        })?
    }

    fn account_token_usage_response(profile: TokenUsageProfile) -> GetAccountTokenUsageResponse {
        let stats = profile.stats;
        GetAccountTokenUsageResponse {
            summary: AccountTokenUsageSummary {
                lifetime_tokens: stats.lifetime_tokens,
                peak_daily_tokens: stats.peak_daily_tokens,
                longest_running_turn_sec: stats.longest_running_turn_sec,
                current_streak_days: stats.current_streak_days,
                longest_streak_days: stats.longest_streak_days,
            },
            daily_usage_buckets: stats.daily_usage_buckets.map(|buckets| {
                buckets
                    .into_iter()
                    .map(|bucket| AccountTokenUsageDailyBucket {
                        start_date: bucket.start_date,
                        tokens: bucket.tokens,
                    })
                    .collect()
            }),
        }
    }

    fn workspace_messages_response(
        messages: BackendWorkspaceMessagesResponse,
        feature_enabled: bool,
    ) -> Result<GetWorkspaceMessagesResponse, JSONRPCErrorError> {
        Ok(GetWorkspaceMessagesResponse {
            feature_enabled,
            messages: messages
                .messages
                .into_iter()
                .map(workspace_message_from_backend)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    async fn send_add_credits_nudge_email_response(
        &self,
        params: SendAddCreditsNudgeEmailParams,
    ) -> Result<SendAddCreditsNudgeEmailResponse, JSONRPCErrorError> {
        self.send_add_credits_nudge_email_inner(params)
            .await
            .map(|status| SendAddCreditsNudgeEmailResponse { status })
    }

    async fn send_add_credits_nudge_email_inner(
        &self,
        params: SendAddCreditsNudgeEmailParams,
    ) -> Result<AddCreditsNudgeEmailStatus, JSONRPCErrorError> {
        let config = self.current_config().await?;
        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to notify workspace owner",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to notify workspace owner",
            ));
        }

        let client = self.backend_client_for_auth(&auth, &config)
            .with_request_timeout(ACCOUNT_BACKEND_REQUEST_TIMEOUT);

        match client
            .send_add_credits_nudge_email(Self::backend_credit_type(params.credit_type))
            .await
        {
            Ok(()) => Ok(AddCreditsNudgeEmailStatus::Sent),
            Err(err) if err.status().is_some_and(|status| status.as_u16() == 429) => {
                Ok(AddCreditsNudgeEmailStatus::CooldownActive)
            }
            Err(err) => Err(internal_error(format!(
                "failed to notify workspace owner: {err}"
            ))),
        }
    }

    fn backend_credit_type(value: AddCreditsNudgeCreditType) -> BackendAddCreditsNudgeCreditType {
        match value {
            AddCreditsNudgeCreditType::Credits => BackendAddCreditsNudgeCreditType::Credits,
            AddCreditsNudgeCreditType::UsageLimit => BackendAddCreditsNudgeCreditType::UsageLimit,
        }
    }
}

fn workspace_message_from_backend(
    message: BackendWorkspaceMessage,
) -> Result<WorkspaceMessage, JSONRPCErrorError> {
    Ok(WorkspaceMessage {
        message_id: message.message_id,
        message_type: workspace_message_type_from_backend(message.message_type),
        message_body: message.message_body,
        created_at: workspace_message_timestamp_from_backend(message.created_at)?,
        archived_at: workspace_message_timestamp_from_backend(message.archived_at)?,
    })
}

fn workspace_message_timestamp_from_backend(
    timestamp: Option<String>,
) -> Result<Option<i64>, JSONRPCErrorError> {
    timestamp
        .map(|timestamp| {
            DateTime::parse_from_rfc3339(&timestamp)
                .map(|timestamp| timestamp.timestamp())
                .map_err(|err| {
                    internal_error(format!(
                        "failed to parse workspace message timestamp `{timestamp}`: {err}"
                    ))
                })
        })
        .transpose()
}

fn workspace_message_type_from_backend(
    message_type: BackendWorkspaceMessageType,
) -> WorkspaceMessageType {
    match message_type {
        BackendWorkspaceMessageType::Headline => WorkspaceMessageType::Headline,
        BackendWorkspaceMessageType::Announcement => WorkspaceMessageType::Announcement,
        BackendWorkspaceMessageType::Unknown => WorkspaceMessageType::Unknown,
    }
}

fn workspace_messages_feature_disabled(err: &BackendRequestError) -> bool {
    err.status().is_some_and(|status| status.as_u16() == 404)
}

