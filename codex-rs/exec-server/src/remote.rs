use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_api::AuthProvider;
use codex_api::SharedAuthProvider;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpResponse;
use futures::FutureExt;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::StatusCode;
use serde::Deserialize;
use tokio::time::sleep;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::debug;
use tracing::info;
use tracing::warn;

use codex_utils_rustls_provider::ensure_rustls_crypto_provider;

use crate::EnvironmentRegistryConnectRequest;
use crate::EnvironmentRegistryConnectResponse;
use crate::EnvironmentRegistryHarnessKeyValidationRequest;
use crate::EnvironmentRegistryHarnessKeyValidationResponse;
use crate::EnvironmentRegistryRegistrationRequest;
use crate::EnvironmentRegistryRegistrationResponse;
use crate::ExecServerError;
use crate::ExecServerRuntimePaths;
use crate::ExecServerTelemetry;
use crate::NoiseChannelIdentity;
use crate::NoiseChannelPublicKey;
use crate::NoiseRendezvousConnectBundle;
use crate::NoiseRendezvousConnectProvider;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_transport::websocket_connector_with_custom_ca;
use crate::noise_relay::noise_relay_websocket_config;
use crate::relay::HarnessKeyValidator;
use crate::relay::run_multiplexed_environment;
use crate::server::ConnectionProcessor;
use crate::trace_context::current_trace_context_headers;

const ERROR_BODY_PREVIEW_BYTES: usize = 4096;
const NOISE_RELAY_SECURITY_PROFILE: &str = "noise_hybrid_ik_v1";
const STABLE_RENDEZVOUS_SESSION: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct EnvironmentRegistryClient {
    base_url: String,
    auth_provider: SharedAuthProvider,
    http: HttpClient,
    connect_timeout: Duration,
    telemetry: ExecServerTelemetry,
}

impl std::fmt::Debug for EnvironmentRegistryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentRegistryClient")
            .field("base_url", &self.base_url)
            .field("auth_provider", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl EnvironmentRegistryClient {
    #[cfg(test)]
    fn new(base_url: String, auth_provider: SharedAuthProvider) -> Result<Self, ExecServerError> {
        Self::new_with_telemetry(base_url, auth_provider, ExecServerTelemetry::default())
    }

    fn new_with_telemetry(
        base_url: String,
        auth_provider: SharedAuthProvider,
        telemetry: ExecServerTelemetry,
    ) -> Result<Self, ExecServerError> {
        let base_url = normalize_base_url(base_url)?;
        Ok(Self {
            base_url,
            auth_provider,
            http: HttpClientBuilder::new()
                .without_redirects()
                .build_with_transport_default_proxy()
                .map_err(|error| ExecServerError::EnvironmentRegistryConfig(error.to_string()))?,
            connect_timeout: DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
            telemetry,
        })
    }

    /// Register the executor public key and obtain the rendezvous allocation.
    /// The returned registration ID is included in each stream's Noise prologue.
    #[tracing::instrument(
        name = "codex.exec_server.remote.register",
        skip_all,
        fields(
            otel.kind = "client",
            otel.name = "codex.exec_server.remote.register",
            result = tracing::field::Empty,
        )
    )]
    async fn register_environment(
        &self,
        environment_id: &str,
        executor_public_key: &NoiseChannelPublicKey,
    ) -> Result<EnvironmentRegistryRegistrationResponse, ExecServerError> {
        let started_at = Instant::now();
        let response = self
            .register_environment_inner(environment_id, executor_public_key)
            .await;
        let result = if response.is_ok() { "success" } else { "error" };
        tracing::Span::current().record("result", result);
        self.telemetry
            .remote_registration_completed(result, started_at.elapsed());
        response
    }

    async fn register_environment_inner(
        &self,
        environment_id: &str,
        executor_public_key: &NoiseChannelPublicKey,
    ) -> Result<EnvironmentRegistryRegistrationResponse, ExecServerError> {
        let response = self
            .http
            .post(endpoint_url(&self.base_url, environment_id, "register")?)
            .timeout(self.connect_timeout)
            .headers(self.auth_provider.to_auth_headers())
            .headers(current_trace_context_headers())
            .json(&EnvironmentRegistryRegistrationRequest {
                security_profile: NOISE_RELAY_SECURITY_PROFILE.to_string(),
                executor_public_key: executor_public_key.clone(),
            })
            .send()
            .await?;
        let response: EnvironmentRegistryRegistrationResponse =
            self.parse_json_response(response).await?;
        if response.executor_registration_id.trim().is_empty() {
            return Err(ExecServerError::Protocol(
                "environment registry returned an empty executor registration id".to_string(),
            ));
        }
        validate_rendezvous_url(&response.url)?;
        if response.environment_id != environment_id {
            return Err(ExecServerError::Protocol(
                "environment registry returned a different environment id".to_string(),
            ));
        }
        if response.security_profile != NOISE_RELAY_SECURITY_PROFILE {
            return Err(ExecServerError::Protocol(format!(
                "environment registry returned unsupported security profile `{}`",
                response.security_profile
            )));
        }
        info!(
            noise_event = "registration",
            noise_outcome = "ok",
            security_profile = NOISE_RELAY_SECURITY_PROFILE,
            "Noise executor registration completed"
        );
        debug!(
            environment_id = response.environment_id,
            executor_registration_id = response.executor_registration_id,
            "Noise executor registration details"
        );
        Ok(response)
    }

    /// Authorize one Noise harness key and obtain the full rendezvous bundle.
    async fn connect_environment(
        &self,
        environment_id: &str,
        harness_public_key: NoiseChannelPublicKey,
    ) -> Result<NoiseRendezvousConnectBundle, ExecServerError> {
        let response = self
            .http
            .post(endpoint_url(&self.base_url, environment_id, "connect")?)
            .headers(self.auth_provider.to_auth_headers())
            .json(&EnvironmentRegistryConnectRequest { harness_public_key })
            .timeout(self.connect_timeout)
            .send()
            .await?;
        let response: EnvironmentRegistryConnectResponse =
            self.parse_json_response(response).await?;
        if response.environment_id != environment_id {
            return Err(ExecServerError::Protocol(
                "environment registry returned a different environment id".to_string(),
            ));
        }
        if response.security_profile != NOISE_RELAY_SECURITY_PROFILE {
            return Err(ExecServerError::Protocol(format!(
                "environment registry returned unsupported security profile `{}`",
                response.security_profile
            )));
        }
        if response.url.trim().is_empty()
            || response.executor_registration_id.trim().is_empty()
            || response.harness_key_authorization.trim().is_empty()
        {
            return Err(ExecServerError::Protocol(
                "environment registry returned incomplete Noise connection data".to_string(),
            ));
        }
        validate_rendezvous_url(&response.url)?;
        Ok(NoiseRendezvousConnectBundle {
            websocket_url: response.url,
            environment_id: response.environment_id,
            executor_registration_id: response.executor_registration_id,
            executor_public_key: response.executor_public_key,
            harness_key_authorization: response.harness_key_authorization,
        })
    }

    async fn parse_json_response<R>(&self, mut response: HttpResponse) -> Result<R, ExecServerError>
    where
        R: for<'de> Deserialize<'de>,
    {
        if response.status().is_success() {
            return response.json::<R>().await.map_err(ExecServerError::from);
        }

        let status = response.status();
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            let remaining = (ERROR_BODY_PREVIEW_BYTES + 1).saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if body.len() > ERROR_BODY_PREVIEW_BYTES {
                break;
            }
        }
        let body = String::from_utf8_lossy(&body);
        // An incomplete body must not be interpreted as a complete registry error.
        let body = if body.len() > ERROR_BODY_PREVIEW_BYTES {
            preview_error_body(&body).unwrap_or_default()
        } else {
            body.into_owned()
        };
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(environment_registry_auth_error(status, &body));
        }

        Err(environment_registry_http_error(status, &body))
    }
}

#[derive(Clone)]
struct RegistryHarnessKeyValidator {
    client: EnvironmentRegistryClient,
    environment_id: String,
    executor_registration_id: String,
}

impl HarnessKeyValidator for RegistryHarnessKeyValidator {
    /// Authorize the harness key recovered from the first IK message.
    /// Noise proves key possession; the registry decides whether that key may use
    /// this executor. The authorization token and public key are checked together.
    async fn validate_harness_key(
        &self,
        harness_public_key: &NoiseChannelPublicKey,
        authorization: &str,
    ) -> Result<(), ExecServerError> {
        let environment_id = &self.environment_id;
        let response = self
            .client
            .http
            .post(endpoint_url(
                &self.client.base_url,
                environment_id,
                "validate",
            )?)
            .headers(self.client.auth_provider.to_auth_headers())
            .json(&EnvironmentRegistryHarnessKeyValidationRequest {
                executor_registration_id: self.executor_registration_id.clone(),
                harness_public_key: harness_public_key.clone(),
                harness_key_authorization: authorization.to_string(),
            })
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            // The request contains the short-lived authorization. Do not include
            // a response body that might echo it in logs or error chains.
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                return Err(ExecServerError::EnvironmentRegistryAuth(format!(
                    "environment registry harness key validation authentication failed ({status})"
                )));
            }
            return Err(ExecServerError::EnvironmentRegistryHttp {
                status,
                code: None,
                message: "environment registry harness key validation failed".to_string(),
            });
        }
        let response = response
            .json::<EnvironmentRegistryHarnessKeyValidationResponse>()
            .await?;
        if !response.valid {
            return Err(ExecServerError::Protocol(
                "environment registry rejected Noise relay harness key".to_string(),
            ));
        }
        Ok(())
    }
}

/// Noise connection configuration for a Codex harness.
///
/// The provider holds the authenticated registry client so every reconnect
/// receives fresh URL and harness-key authorization material.
#[derive(Clone)]
pub(crate) struct NoiseRendezvousEnvironmentConfig {
    provider: Arc<dyn NoiseRendezvousConnectProvider>,
}

impl std::fmt::Debug for NoiseRendezvousEnvironmentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoiseRendezvousEnvironmentConfig")
            .field("provider", &"<redacted>")
            .finish()
    }
}

impl NoiseRendezvousEnvironmentConfig {
    pub(crate) fn new(
        base_url: String,
        environment_id: String,
        bearer_token: String,
        chatgpt_account_id: Option<String>,
    ) -> Result<Self, ExecServerError> {
        let environment_id = normalize_environment_id(environment_id)?;
        let auth_provider = static_bearer_auth_provider(bearer_token, chatgpt_account_id)?;
        let client = EnvironmentRegistryClient::new_with_telemetry(
            base_url,
            auth_provider,
            ExecServerTelemetry::default(),
        )?;
        Ok(Self {
            provider: Arc::new(EnvironmentRegistryNoiseConnectProvider {
                client,
                environment_id,
            }),
        })
    }

    pub(crate) fn connect_provider(&self) -> Arc<dyn NoiseRendezvousConnectProvider> {
        Arc::clone(&self.provider)
    }
}

#[derive(Clone, Debug)]
struct EnvironmentRegistryNoiseConnectProvider {
    client: EnvironmentRegistryClient,
    environment_id: String,
}

impl NoiseRendezvousConnectProvider for EnvironmentRegistryNoiseConnectProvider {
    fn connect_bundle(
        &self,
        harness_public_key: NoiseChannelPublicKey,
    ) -> futures::future::BoxFuture<'_, Result<NoiseRendezvousConnectBundle, ExecServerError>> {
        async move {
            self.client
                .connect_environment(&self.environment_id, harness_public_key)
                .await
        }
        .boxed()
    }
}

#[derive(Clone)]
struct StaticBearerAuthProvider {
    authorization: HeaderValue,
    chatgpt_account_id: Option<HeaderValue>,
}

impl std::fmt::Debug for StaticBearerAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticBearerAuthProvider")
            .field("authorization", &"<redacted>")
            .field(
                "chatgpt_account_id",
                &self.chatgpt_account_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl AuthProvider for StaticBearerAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.insert(http::header::AUTHORIZATION, self.authorization.clone());
        if let Some(chatgpt_account_id) = &self.chatgpt_account_id {
            headers.insert(
                HeaderName::from_static("chatgpt-account-id"),
                chatgpt_account_id.clone(),
            );
        }
    }
}

fn static_bearer_auth_provider(
    bearer_token: String,
    chatgpt_account_id: Option<String>,
) -> Result<SharedAuthProvider, ExecServerError> {
    let bearer_token = bearer_token.trim();
    if bearer_token.is_empty() {
        return Err(ExecServerError::EnvironmentRegistryConfig(
            "environment registry bearer token is required".to_string(),
        ));
    }
    let authorization =
        HeaderValue::try_from(format!("Bearer {bearer_token}")).map_err(|error| {
            ExecServerError::EnvironmentRegistryConfig(format!(
                "environment registry bearer token is not a valid HTTP header: {error}"
            ))
        })?;
    let chatgpt_account_id = chatgpt_account_id
        .as_deref()
        .map(str::trim)
        .filter(|account_id| !account_id.is_empty())
        .map(|account_id| {
            HeaderValue::try_from(account_id).map_err(|error| {
                ExecServerError::EnvironmentRegistryConfig(format!(
                    "ChatGPT account id is not a valid HTTP header: {error}"
                ))
            })
        })
        .transpose()?;
    Ok(Arc::new(StaticBearerAuthProvider {
        authorization,
        chatgpt_account_id,
    }))
}

/// Configuration for registering an exec-server for remote use.
#[derive(Clone)]
pub struct RemoteEnvironmentConfig {
    pub base_url: String,
    pub environment_id: String,
    pub name: String,
    auth_provider: SharedAuthProvider,
    telemetry: ExecServerTelemetry,
}

impl std::fmt::Debug for RemoteEnvironmentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEnvironmentConfig")
            .field("base_url", &self.base_url)
            .field("environment_id", &self.environment_id)
            .field("name", &self.name)
            .field("auth_provider", &"<redacted>")
            .finish()
    }
}

impl RemoteEnvironmentConfig {
    pub fn new(
        base_url: String,
        environment_id: String,
        auth_provider: SharedAuthProvider,
    ) -> Result<Self, ExecServerError> {
        let environment_id = normalize_environment_id(environment_id)?;
        Ok(Self {
            base_url,
            environment_id,
            name: "codex-exec-server".to_string(),
            auth_provider,
            telemetry: ExecServerTelemetry::default(),
        })
    }

    pub fn with_telemetry(mut self, telemetry: ExecServerTelemetry) -> Self {
        self.telemetry = telemetry;
        self
    }
}

/// Register an exec-server for remote use and serve requests over Noise.
///
/// The executor identity is generated once per process and reused across
/// reconnects. The registration and rendezvous URL are also reused until
/// rendezvous rejects the URL, at which point the next attempt registers again.
/// The websocket carries cleartext routing metadata and encrypted payloads.
pub async fn run_remote_environment(
    config: RemoteEnvironmentConfig,
    runtime_paths: ExecServerRuntimePaths,
) -> Result<(), ExecServerError> {
    ensure_rustls_crypto_provider();
    let client = EnvironmentRegistryClient::new_with_telemetry(
        config.base_url.clone(),
        config.auth_provider.clone(),
        config.telemetry.clone(),
    )?;
    let processor =
        ConnectionProcessor::new_with_telemetry(runtime_paths, config.telemetry.clone());
    let identity = NoiseChannelIdentity::generate().map_err(|error| {
        ExecServerError::Protocol(format!("failed to generate Noise relay identity: {error}"))
    })?;
    let mut backoff = Duration::from_secs(1);
    let mut response = register_with_retry(
        &client,
        &config.environment_id,
        &identity.public_key(),
        &mut backoff,
    )
    .await?;

    loop {
        match connect_rendezvous(&response.url, &config.telemetry).await {
            Ok(websocket) => {
                let connected_at = Instant::now();
                let executor_registration_id = response.executor_registration_id.clone();
                info!(
                    noise_event = "rendezvous_connection",
                    noise_outcome = "ok",
                    "Noise executor connected to rendezvous"
                );
                let disconnect_reason = run_multiplexed_environment(
                    websocket,
                    processor.clone(),
                    response.environment_id.clone(),
                    executor_registration_id.clone(),
                    identity.clone(),
                    RegistryHarnessKeyValidator {
                        client: client.clone(),
                        environment_id: config.environment_id.clone(),
                        executor_registration_id,
                    },
                )
                .await;
                if connected_at.elapsed() >= STABLE_RENDEZVOUS_SESSION {
                    backoff = Duration::from_secs(1);
                }
                info!(
                    noise_event = "rendezvous_connection",
                    noise_outcome = "disconnected",
                    noise_reason = disconnect_reason.as_str(),
                    "Noise executor disconnected from rendezvous"
                );
                config
                    .telemetry
                    .remote_reconnect(disconnect_reason.as_str());
            }
            Err(error) => {
                let registration_rejected = matches!(
                    &error,
                    tokio_tungstenite::tungstenite::Error::Http(response)
                        if matches!(response.status().as_u16(), 401 | 403 | 404 | 410)
                );
                warn!(
                    noise_event = "rendezvous_connection",
                    noise_outcome = "error",
                    noise_reason = "websocket_error",
                    "Noise executor failed to connect to rendezvous"
                );
                debug!(error = %error, "Noise executor rendezvous connection error");
                if registration_rejected {
                    config.telemetry.remote_reconnect("registration_rejected");
                    response = register_with_retry(
                        &client,
                        &config.environment_id,
                        &identity.public_key(),
                        &mut backoff,
                    )
                    .await?;
                } else {
                    config.telemetry.remote_reconnect("connect_failed");
                }
            }
        }

        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn register_with_retry(
    client: &EnvironmentRegistryClient,
    environment_id: &str,
    public_key: &NoiseChannelPublicKey,
    backoff: &mut Duration,
) -> Result<EnvironmentRegistryRegistrationResponse, ExecServerError> {
    loop {
        match client
            .register_environment(environment_id, public_key)
            .await
        {
            Ok(response) => return Ok(response),
            Err(error)
                if matches!(&error,
                    ExecServerError::EnvironmentRegistryRequest(error) if error.is_connect() || error.is_timeout()
                ) || matches!(&error,
                    ExecServerError::EnvironmentRegistryHttp { status, .. }
                        if status.is_server_error() || matches!(status.as_u16(), 408 | 429)
                ) =>
            {
                warn!("transient environment registration failure; retrying after backoff");
                sleep(*backoff).await;
                *backoff = (*backoff * 2).min(Duration::from_secs(30));
            }
            Err(error) => return Err(error),
        }
    }
}

#[tracing::instrument(
    name = "codex.exec_server.remote.rendezvous.connect",
    skip_all,
    fields(
        otel.kind = "client",
        otel.name = "codex.exec_server.remote.rendezvous.connect",
        result = tracing::field::Empty,
    )
)]
async fn connect_rendezvous(
    url: &str,
    telemetry: &ExecServerTelemetry,
) -> Result<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let started_at = Instant::now();
    let result = tokio::time::timeout(DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT, async {
        let connector = websocket_connector_with_custom_ca(url).await?;
        let mut request = url.into_client_request()?;
        request
            .headers_mut()
            .extend(current_trace_context_headers());
        connect_async_tls_with_config(
            request,
            Some(noise_relay_websocket_config()),
            // Rendezvous sends small, latency-sensitive frames, so avoid Nagle's coalescing delay.
            /*disable_nagle*/
            true,
            connector,
        )
        .await
        .map(|(websocket, _)| websocket)
    })
    .await
    .unwrap_or_else(|_| {
        Err(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out connecting to rendezvous",
            ),
        ))
    });
    let result_name = if result.is_ok() { "success" } else { "error" };
    tracing::Span::current().record("result", result_name);
    telemetry.remote_rendezvous_completed(result_name, started_at.elapsed());
    result
}

fn validate_rendezvous_url(value: &str) -> Result<(), ExecServerError> {
    let url = url::Url::parse(value).map_err(|_| {
        ExecServerError::Protocol(
            "environment registry returned an invalid rendezvous URL".to_string(),
        )
    })?;
    if !matches!(url.scheme(), "ws" | "wss") || url.host_str().is_none() {
        return Err(ExecServerError::Protocol(
            "environment registry returned an invalid rendezvous URL".to_string(),
        ));
    }
    value.into_client_request().map_err(|_| {
        ExecServerError::Protocol(
            "environment registry returned an invalid rendezvous URL".to_string(),
        )
    })?;
    Ok(())
}

fn normalize_environment_id(environment_id: String) -> Result<String, ExecServerError> {
    let environment_id = environment_id.trim().to_string();
    if environment_id.is_empty() {
        return Err(ExecServerError::EnvironmentRegistryConfig(
            "environment id is required for remote exec-server registration".to_string(),
        ));
    }
    Ok(environment_id)
}

#[derive(Deserialize)]
struct RegistryErrorBody {
    error: Option<RegistryError>,
}

#[derive(Deserialize)]
struct RegistryError {
    code: Option<String>,
    message: Option<String>,
}

fn normalize_base_url(base_url: String) -> Result<String, ExecServerError> {
    let trimmed = base_url.trim().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return Err(ExecServerError::EnvironmentRegistryConfig(
            "environment registry base URL is required".to_string(),
        ));
    }
    Ok(trimmed)
}

fn endpoint_url(
    base_url: &str,
    environment_id: &str,
    operation: &str,
) -> Result<String, ExecServerError> {
    // URL path builders normalize dot segments; they cannot identify an environment.
    if matches!(environment_id, "." | "..") {
        return Err(ExecServerError::EnvironmentRegistryConfig(
            "environment id must not be a URL dot segment".to_string(),
        ));
    }
    let mut url = url::Url::parse(base_url)
        .map_err(|error| ExecServerError::EnvironmentRegistryConfig(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| {
            ExecServerError::EnvironmentRegistryConfig(
                "environment registry URL must support path segments".to_string(),
            )
        })?
        .pop_if_empty()
        .extend(["cloud", "environment", environment_id, operation]);
    Ok(url.into())
}

fn environment_registry_auth_error(status: StatusCode, body: &str) -> ExecServerError {
    let message = registry_error_message(body).unwrap_or_else(|| "empty error body".to_string());
    ExecServerError::EnvironmentRegistryAuth(format!(
        "environment registry authentication failed ({status}): {message}"
    ))
}

fn environment_registry_http_error(status: StatusCode, body: &str) -> ExecServerError {
    let parsed = serde_json::from_str::<RegistryErrorBody>(body).ok();
    let (code, message) = parsed
        .and_then(|body| body.error)
        .map(|error| {
            (
                error.code.and_then(|code| preview_error_body(&code)),
                error
                    .message
                    .and_then(|message| preview_error_body(&message))
                    .unwrap_or_else(|| {
                        preview_error_body(body).unwrap_or_else(|| "empty error body".to_string())
                    }),
            )
        })
        .unwrap_or_else(|| {
            (
                None,
                preview_error_body(body)
                    .unwrap_or_else(|| "empty or malformed error body".to_string()),
            )
        });
    ExecServerError::EnvironmentRegistryHttp {
        status,
        code,
        message,
    }
}

fn registry_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<RegistryErrorBody>(body)
        .ok()
        .and_then(|body| body.error)
        .and_then(|error| error.message)
        .and_then(|message| preview_error_body(&message))
        .or_else(|| preview_error_body(body))
}

fn preview_error_body(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= ERROR_BODY_PREVIEW_BYTES {
        return Some(trimmed.to_string());
    }
    const SUFFIX: &str = " [truncated]";
    let mut end = ERROR_BODY_PREVIEW_BYTES - SUFFIX.len();
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}{SUFFIX}", &trimmed[..end]))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_api::AuthProvider;
    use http::HeaderMap;
    use http::HeaderValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use pretty_assertions::assert_eq;
    use tracing::Instrument;
    use tracing_subscriber::prelude::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_partial_json;
    use wiremock::matchers::header;
    use wiremock::matchers::header_regex;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::*;

    #[derive(Debug)]
    struct StaticRegistryAuthProvider;

    impl AuthProvider for StaticRegistryAuthProvider {
        fn add_auth_headers(&self, headers: &mut HeaderMap) {
            let _ = headers.insert(
                http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer registry-token"),
            );
            let _ = headers.insert(
                "ChatGPT-Account-ID",
                HeaderValue::from_static("workspace-123"),
            );
        }
    }

    fn static_registry_auth_provider() -> SharedAuthProvider {
        Arc::new(StaticRegistryAuthProvider)
    }

    #[tokio::test]
    async fn registration_retries_transient_failures_but_stops_on_auth_failure() {
        for terminal in [false, true] {
            let server = MockServer::start().await;
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = Arc::clone(&calls);
            Mock::given(method("POST"))
                .and(path("/cloud/environment/test/register"))
                .respond_with(move |_request: &wiremock::Request| {
                    let attempt = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if terminal { return ResponseTemplate::new(401); }
                    if attempt == 0 { return ResponseTemplate::new(503); }
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "environment_id": "test", "url": "wss://rendezvous.test/connection",
                        "security_profile": "noise_hybrid_ik_v1", "executor_registration_id": "registration-1",
                    }))
                }).mount(&server).await;
            let client =
                EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
                    .unwrap();
            let key = NoiseChannelIdentity::generate().unwrap().public_key();
            let mut backoff = Duration::from_millis(1);
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                register_with_retry(&client, "test", &key, &mut backoff),
            )
            .await
            .unwrap();
            if terminal {
                assert!(matches!(
                    result,
                    Err(ExecServerError::EnvironmentRegistryAuth(_))
                ));
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            } else {
                assert_eq!(result.unwrap().executor_registration_id, "registration-1");
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
                assert_eq!(backoff, Duration::from_millis(2));
            }
        }
    }

    #[tokio::test]
    async fn registry_operations_encode_environment_id_segments() {
        for (environment_id, encoded_id) in [
            ("env-name_1.~", "env-name_1.~"),
            ("team/name?# %", "team%2Fname%3F%23%20%25"),
        ] {
            let server = MockServer::start().await;
            let public_key = NoiseChannelIdentity::generate()
                .expect("identity")
                .public_key();
            for operation in ["register", "connect", "validate"] {
                Mock::given(method("POST"))
                    .and(path(format!(
                        "/registry/cloud/environment/{encoded_id}/{operation}"
                    )))
                    .and(header("authorization", "Bearer registry-token"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "environment_id": environment_id,
                        "url": "wss://rendezvous.test/connection",
                        "security_profile": "noise_hybrid_ik_v1",
                        "executor_registration_id": "registration-1",
                        "executor_public_key": public_key.clone(),
                        "harness_key_authorization": "authorization-1",
                        "valid": true,
                    })))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            let base_url = format!("{}/registry/", server.uri());
            let client =
                EnvironmentRegistryClient::new(base_url.clone(), static_registry_auth_provider())
                    .expect("client");
            let registration = client
                .register_environment(environment_id, &public_key)
                .await
                .expect("register encoded environment");
            assert_eq!(registration.environment_id, environment_id);

            let config = NoiseRendezvousEnvironmentConfig::new(
                base_url,
                environment_id.to_string(),
                "registry-token".to_string(),
                None,
            )
            .expect("noise configuration");
            let bundle = config
                .connect_provider()
                .connect_bundle(public_key.clone())
                .await
                .expect("connect encoded environment");
            assert_eq!(bundle.environment_id, environment_id);
            assert_eq!(bundle.harness_key_authorization, "authorization-1");

            RegistryHarnessKeyValidator {
                client,
                environment_id: environment_id.to_string(),
                executor_registration_id: "registration-1".to_string(),
            }
            .validate_harness_key(&public_key, "authorization-1")
            .await
            .expect("validate encoded environment");
            let requests = server.received_requests().await.expect("recorded requests");
            assert_eq!(requests.len(), 3);
            for request in requests {
                assert_eq!(request.url.query(), None);
                assert_eq!(request.url.fragment(), None);
            }
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn registry_operations_reject_dot_segment_environment_ids_without_requests() {
        let server = MockServer::start().await;
        let client = EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
            .expect("client");
        let public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        for environment_id in [".", ".."] {
            let registration = client
                .register_environment(environment_id, &public_key)
                .await;
            assert!(matches!(
                registration,
                Err(ExecServerError::EnvironmentRegistryConfig(_))
            ));
            let connection = client
                .connect_environment(environment_id, public_key.clone())
                .await;
            assert!(matches!(
                connection,
                Err(ExecServerError::EnvironmentRegistryConfig(_))
            ));
            let validation = RegistryHarnessKeyValidator {
                client: client.clone(),
                environment_id: environment_id.to_string(),
                executor_registration_id: "registration-1".to_string(),
            }
            .validate_harness_key(&public_key, "authorization-1")
            .await;
            assert!(matches!(
                validation,
                Err(ExecServerError::EnvironmentRegistryConfig(_))
            ));
        }
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded requests")
                .is_empty()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn register_environment_posts_with_auth_provider_headers() {
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("exec-server-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let _guard = subscriber.set_default();
        tracing::callsite::rebuild_interest_cache();
        let server = MockServer::start().await;
        let executor_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/register"))
            .and(header("authorization", "Bearer registry-token"))
            .and(header("chatgpt-account-id", "workspace-123"))
            .and(header_regex(
                "traceparent",
                "^00-[0-9a-f]{32}-[0-9a-f]{16}-0[01]$",
            ))
            .and(body_partial_json(serde_json::json!({
                "security_profile": NOISE_RELAY_SECURITY_PROFILE,
                "executor_public_key": executor_public_key.clone(),
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "environment_id": "environment-requested",
                "url": "wss://rendezvous.test/cloud-agent/default/ws/environment/environment-requested?role=environment&sig=abc",
                "security_profile": NOISE_RELAY_SECURITY_PROFILE,
                "executor_registration_id": "registration-1",
            })))
            .mount(&server)
            .await;
        let client = EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
            .expect("client");

        let response = client
            .register_environment("environment-requested", &executor_public_key)
            .instrument(tracing::info_span!("remote-operation"))
            .await
            .expect("register environment");

        assert_eq!(
            response,
            EnvironmentRegistryRegistrationResponse {
                environment_id: "environment-requested".to_string(),
                url: "wss://rendezvous.test/cloud-agent/default/ws/environment/environment-requested?role=environment&sig=abc".to_string(),
                security_profile: NOISE_RELAY_SECURITY_PROFILE.to_string(),
                executor_registration_id: "registration-1".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn noise_connect_provider_requests_and_validates_a_full_bundle() {
        let server = MockServer::start().await;
        let harness_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        let executor_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/connect"))
            .and(header("authorization", "Bearer registry-token"))
            .and(header("chatgpt-account-id", "workspace-123"))
            .and(body_partial_json(serde_json::json!({
                "harness_public_key": harness_public_key.clone(),
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "environment_id": "environment-requested",
                "url": "wss://rendezvous.test/cloud-agent/default/ws/environment/environment-requested?role=harness&sig=abc",
                "security_profile": NOISE_RELAY_SECURITY_PROFILE,
                "executor_registration_id": "registration-1",
                "executor_public_key": executor_public_key.clone(),
                "harness_key_authorization": "authorization-1",
            })))
            .mount(&server)
            .await;
        let config = NoiseRendezvousEnvironmentConfig::new(
            server.uri(),
            "environment-requested".to_string(),
            "registry-token".to_string(),
            Some("workspace-123".to_string()),
        )
        .expect("noise configuration");

        let bundle = config
            .connect_provider()
            .connect_bundle(harness_public_key)
            .await
            .expect("Noise connect bundle");

        assert_eq!(
            bundle.websocket_url,
            "wss://rendezvous.test/cloud-agent/default/ws/environment/environment-requested?role=harness&sig=abc"
        );
        assert_eq!(bundle.environment_id, "environment-requested");
        assert_eq!(bundle.executor_registration_id, "registration-1");
        assert_eq!(bundle.executor_public_key, executor_public_key);
        assert_eq!(bundle.harness_key_authorization, "authorization-1");
    }

    #[test_case::test_case(false; "connect")]
    #[test_case::test_case(true; "register")]
    #[tokio::test]
    async fn registry_request_times_out_when_registry_stalls(register: bool) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/cloud/environment/environment-requested/{}",
                if register { "register" } else { "connect" }
            )))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
            .mount(&server)
            .await;
        let mut client =
            EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
                .expect("client");
        client.connect_timeout = Duration::from_millis(50);
        let harness_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();

        let result = if register {
            client
                .register_environment("environment-requested", &harness_public_key)
                .await
                .map(|_| ())
        } else {
            client
                .connect_environment("environment-requested", harness_public_key)
                .await
                .map(|_| ())
        };
        let error = match result {
            Ok(_) => panic!("stalled connect response should time out"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ExecServerError::EnvironmentRegistryRequest(error) if error.is_timeout()
        ));
    }

    #[test_case::test_case("", "wss://rendezvous.test/ws"; "missing_registration")]
    #[test_case::test_case("   ", "wss://rendezvous.test/ws"; "blank_registration")]
    #[test_case::test_case("registration-1", ""; "missing_url")]
    #[test_case::test_case("registration-1", "https://rendezvous.test/ws"; "wrong_scheme")]
    #[test_case::test_case("registration-1", "ws://"; "missing_host")]
    #[tokio::test]
    async fn register_rejects_unusable_connection_data(registration: &str, url: &str) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/register"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "environment_id": "environment-requested",
                "url": url,
                "security_profile": NOISE_RELAY_SECURITY_PROFILE,
                "executor_registration_id": registration,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
            .expect("client");
        let public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        let result = client
            .register_environment("environment-requested", &public_key)
            .await;
        assert!(matches!(result, Err(ExecServerError::Protocol(_))));
    }

    #[tokio::test]
    async fn registry_error_body_is_bounded_before_response_finishes() -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0; 8192];
            let read = stream.read(&mut request).await?;
            assert!(
                read > 0,
                "client must send a request before the error response"
            );
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 1000000\r\n\r\n")
                .await?;
            // Non-ASCII content also proves the diagnostic limit counts UTF-8 bytes.
            stream.write_all("\u{e9}".repeat(3000).as_bytes()).await?;
            let _ = release_rx.await;
            Ok::<_, std::io::Error>(())
        });
        let client = EnvironmentRegistryClient::new(url, static_registry_auth_provider())?;
        let key = NoiseChannelIdentity::generate()?.public_key();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.register_environment("environment-requested", &key),
        )
        .await;
        let _ = release_tx.send(());
        server.await??;
        match result? {
            Err(ExecServerError::EnvironmentRegistryHttp {
                status, message, ..
            }) => {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
                assert!(message.len() <= ERROR_BODY_PREVIEW_BYTES);
                assert!(message.starts_with('\u{e9}'));
                assert!(message.ends_with(" [truncated]"));
            }
            other => panic!("expected bounded HTTP error, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn rendezvous_connection_times_out_during_websocket_upgrade() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let connection =
            tokio::spawn(
                async move { connect_rendezvous(&url, &ExecServerTelemetry::default()).await },
            );
        let (_stream, _) = listener.accept().await?;
        tokio::time::pause();
        tokio::time::advance(DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT).await;
        let result = tokio::time::timeout(Duration::from_secs(1), connection).await??;
        assert!(
            matches!(result, Err(tokio_tungstenite::tungstenite::Error::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        Ok(())
    }

    #[tokio::test]
    async fn repeated_short_rendezvous_sessions_increase_backoff() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let registry = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/register"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "environment_id": "environment-requested",
                "url": format!("ws://{}", listener.local_addr()?),
                "security_profile": NOISE_RELAY_SECURITY_PROFILE,
                "executor_registration_id": "registration-1",
            })))
            .expect(1)
            .mount(&registry)
            .await;
        let config = RemoteEnvironmentConfig::new(
            registry.uri(),
            "environment-requested".into(),
            static_registry_auth_provider(),
        )?;
        let runtime_paths = ExecServerRuntimePaths::new(std::env::current_exe()?)?;
        let task = tokio::spawn(run_remote_environment(config, runtime_paths));
        let result = async {
            for _ in 0..2 {
                let (stream, _) =
                    tokio::time::timeout(Duration::from_secs(3), listener.accept()).await??;
                let mut websocket = tokio_tungstenite::accept_async(stream).await?;
                websocket.close(None).await?;
            }
            // The next delay is two seconds even though both upgrades succeeded.
            assert!(
                tokio::time::timeout(Duration::from_millis(1500), listener.accept())
                    .await
                    .is_err()
            );
            let (stream, _) =
                tokio::time::timeout(Duration::from_secs(2), listener.accept()).await??;
            let _websocket = tokio_tungstenite::accept_async(stream).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        task.abort();
        let _ = task.await;
        result
    }

    #[tokio::test]
    async fn register_environment_does_not_follow_redirects_with_auth_headers() {
        let server = MockServer::start().await;
        let executor_public_key = NoiseChannelIdentity::generate()
            .expect("identity")
            .public_key();
        Mock::given(method("POST"))
            .and(path("/cloud/environment/environment-requested/register"))
            .and(header("authorization", "Bearer registry-token"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/redirect-target", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(path("/redirect-target"))
            .and(header("authorization", "Bearer registry-token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let client = EnvironmentRegistryClient::new(server.uri(), static_registry_auth_provider())
            .expect("client");

        let error = client
            .register_environment("environment-requested", &executor_public_key)
            .await
            .expect_err("redirect response should not be followed");

        assert!(matches!(
            error,
            ExecServerError::EnvironmentRegistryHttp {
                status: StatusCode::FOUND,
                ..
            }
        ));
    }

    #[test]
    fn debug_output_redacts_auth_provider() {
        let config = RemoteEnvironmentConfig::new(
            "https://registry.example".to_string(),
            "env-1".to_string(),
            static_registry_auth_provider(),
        )
        .expect("config");

        let debug = format!("{config:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("workspace-123"));
    }
}

#[cfg(test)]
#[path = "remote/noise_tests.rs"]
mod noise_tests;
