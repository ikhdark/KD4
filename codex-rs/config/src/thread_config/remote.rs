use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use codex_model_provider_info::ModelProviderAwsAuthInfo;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::config_types::ModelProviderAuthInfo;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::SessionThreadConfig;
use super::ThreadConfigContext;
use super::ThreadConfigLoadError;
use super::ThreadConfigLoadErrorCode;
use super::ThreadConfigLoader;
use super::ThreadConfigLoaderFuture;
use super::ThreadConfigSource;
use super::UserThreadConfig;
use proto::thread_config_loader_client::ThreadConfigLoaderClient;

#[path = "proto/codex.thread_config.v1.rs"]
mod proto;

const REMOTE_THREAD_CONFIG_LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// gRPC-backed [`ThreadConfigLoader`] implementation.
#[derive(Clone, Debug)]
pub struct RemoteThreadConfigLoader {
    endpoint: String,
    channel: Arc<tokio::sync::OnceCell<tonic::transport::Channel>>,
}

impl RemoteThreadConfigLoader {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            channel: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    async fn client(
        &self,
    ) -> Result<ThreadConfigLoaderClient<tonic::transport::Channel>, ThreadConfigLoadError> {
        let channel = self
            .channel
            .get_or_try_init(|| async {
                tonic::transport::Endpoint::from_shared(self.endpoint.clone())
                    .map_err(connection_error)?
                    .connect()
                    .await
                    .map_err(connection_error)
            })
            .await?;
        Ok(ThreadConfigLoaderClient::new(channel.clone()))
    }

    async fn load(
        &self,
        context: ThreadConfigContext,
    ) -> Result<Vec<ThreadConfigSource>, ThreadConfigLoadError> {
        let deadline = tokio::time::Instant::now() + REMOTE_THREAD_CONFIG_LOAD_TIMEOUT;
        let response = tokio::time::timeout_at(deadline, async {
            let mut client = self.client().await?;
            let request = load_thread_config_request(
                context,
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            );
            client.load(request).await.map_err(remote_status_to_error)
        })
        .await
        .map_err(|_| {
            ThreadConfigLoadError::new(
                ThreadConfigLoadErrorCode::Timeout,
                None,
                "remote thread config load timed out",
            )
        })??
        .into_inner();

        response
            .sources
            .into_iter()
            .map(thread_config_source_from_proto)
            .collect()
    }
}

impl ThreadConfigLoader for RemoteThreadConfigLoader {
    fn load(
        &self,
        context: ThreadConfigContext,
    ) -> ThreadConfigLoaderFuture<'_, Vec<ThreadConfigSource>> {
        Box::pin(RemoteThreadConfigLoader::load(self, context))
    }
}

fn load_thread_config_request(
    context: ThreadConfigContext,
    timeout: Duration,
) -> tonic::Request<proto::LoadThreadConfigRequest> {
    let mut request = tonic::Request::new(proto::LoadThreadConfigRequest {
        thread_id: context.thread_id,
        cwd: context.cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
    });
    request.set_timeout(timeout);
    request
}

fn connection_error(err: tonic::transport::Error) -> ThreadConfigLoadError {
    ThreadConfigLoadError::new(
        ThreadConfigLoadErrorCode::RequestFailed,
        None,
        format!("failed to connect to remote thread config loader: {err}"),
    )
}

fn remote_status_to_error(status: tonic::Status) -> ThreadConfigLoadError {
    // Tonic maps its local transport deadline to Cancelled but retains the
    // typed cause. A server cancellation alone must remain RequestFailed.
    let mut source = std::error::Error::source(&status);
    while let Some(error) = source {
        if error.is::<tonic::TimeoutExpired>() {
            return ThreadConfigLoadError::new(
                ThreadConfigLoadErrorCode::Timeout,
                None,
                format!("remote thread config request timed out: {status}"),
            )
            .with_grpc_code(status.code());
        }
        source = error.source();
    }
    let code = match status.code() {
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
            ThreadConfigLoadErrorCode::Auth
        }
        tonic::Code::DeadlineExceeded => ThreadConfigLoadErrorCode::Timeout,
        tonic::Code::Ok
        | tonic::Code::Cancelled
        | tonic::Code::Unknown
        | tonic::Code::InvalidArgument
        | tonic::Code::NotFound
        | tonic::Code::AlreadyExists
        | tonic::Code::ResourceExhausted
        | tonic::Code::FailedPrecondition
        | tonic::Code::Aborted
        | tonic::Code::OutOfRange
        | tonic::Code::Unimplemented
        | tonic::Code::Internal
        | tonic::Code::Unavailable
        | tonic::Code::DataLoss => ThreadConfigLoadErrorCode::RequestFailed,
    };
    ThreadConfigLoadError::new(
        code,
        /*status_code*/ None,
        format!("remote thread config request failed: {status}"),
    )
    .with_grpc_code(status.code())
}

fn thread_config_source_from_proto(
    source: proto::ThreadConfigSource,
) -> Result<ThreadConfigSource, ThreadConfigLoadError> {
    match source.source {
        Some(proto::thread_config_source::Source::Session(config)) => {
            session_thread_config_from_proto(config).map(ThreadConfigSource::Session)
        }
        Some(proto::thread_config_source::Source::User(_)) => {
            Ok(ThreadConfigSource::User(UserThreadConfig::default()))
        }
        None => Err(parse_error("remote thread config omitted source payload")),
    }
}

fn session_thread_config_from_proto(
    config: proto::SessionThreadConfig,
) -> Result<SessionThreadConfig, ThreadConfigLoadError> {
    let mut model_providers = HashMap::new();
    for provider in config.model_providers {
        let (id, provider) = model_provider_from_proto(provider)?;
        match model_providers.entry(id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(provider);
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                return Err(parse_error(format!(
                    "remote thread config returned duplicate model provider id {:?}",
                    entry.key()
                )));
            }
        }
    }

    Ok(SessionThreadConfig {
        model_provider: config.model_provider,
        model_providers,
        features: config.features.into_iter().collect::<BTreeMap<_, _>>(),
    })
}

fn model_provider_from_proto(
    provider: proto::ModelProvider,
) -> Result<(String, ModelProviderInfo), ThreadConfigLoadError> {
    if provider.id.is_empty() {
        return Err(parse_error(
            "remote thread config returned model provider without an id",
        ));
    }
    let id = provider.id;
    let wire_api = match proto::WireApi::try_from(provider.wire_api) {
        Ok(proto::WireApi::Responses) => WireApi::Responses,
        Ok(proto::WireApi::Unspecified) => {
            return Err(parse_error("remote thread config omitted wire_api"));
        }
        Err(_) => {
            return Err(parse_error(format!(
                "remote thread config returned unknown wire_api: {}",
                provider.wire_api
            )));
        }
    };
    let info = ModelProviderInfo {
        name: provider.name,
        base_url: provider.base_url,
        env_key: provider.env_key,
        env_key_instructions: provider.env_key_instructions,
        experimental_bearer_token: provider.experimental_bearer_token,
        auth: provider
            .auth
            .map(model_provider_auth_from_proto)
            .transpose()?,
        aws: provider.aws.map(|aws| ModelProviderAwsAuthInfo {
            profile: aws.profile,
            region: aws.region,
        }),
        wire_api,
        query_params: provider.query_params.map(|map| map.values),
        http_headers: provider.http_headers.map(|map| map.values),
        env_http_headers: provider.env_http_headers.map(|map| map.values),
        request_max_retries: provider.request_max_retries,
        stream_max_retries: provider.stream_max_retries,
        stream_idle_timeout_ms: provider.stream_idle_timeout_ms,
        websocket_connect_timeout_ms: provider.websocket_connect_timeout_ms,
        requires_openai_auth: provider.requires_openai_auth,
        supports_websockets: provider.supports_websockets,
        request_output_logprobs: provider.request_output_logprobs,
        kind: Default::default(),
        supports_standalone_web_search: provider.supports_standalone_web_search,
        // This remote source does not advertise the optional hosted capability.
        supports_hosted_web_search: false,
    };
    Ok((id, info))
}

fn model_provider_auth_from_proto(
    auth: proto::ModelProviderAuthInfo,
) -> Result<ModelProviderAuthInfo, ThreadConfigLoadError> {
    let timeout_ms = NonZeroU64::new(auth.timeout_ms)
        .ok_or_else(|| parse_error("remote thread config returned zero auth timeout_ms"))?;
    let cwd = AbsolutePathBuf::from_absolute_path_checked(&auth.cwd).map_err(|err| {
        parse_error(format!(
            "remote thread config returned invalid auth cwd {:?}: {err}",
            auth.cwd
        ))
    })?;

    Ok(ModelProviderAuthInfo {
        command: auth.command,
        args: auth.args,
        timeout_ms,
        refresh_interval_ms: auth.refresh_interval_ms,
        cwd,
    })
}

fn parse_error(message: impl Into<String>) -> ThreadConfigLoadError {
    ThreadConfigLoadError::new(
        ThreadConfigLoadErrorCode::Parse,
        /*status_code*/ None,
        message.into(),
    )
}
