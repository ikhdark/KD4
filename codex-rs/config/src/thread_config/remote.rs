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
        supports_standalone_web_search: provider.supports_standalone_web_search,
    };
    Ok((id, info))
}

#[cfg(test)]
fn model_provider_to_proto(
    id: impl Into<String>,
    provider: ModelProviderInfo,
) -> proto::ModelProvider {
    let ModelProviderInfo {
        name,
        base_url,
        env_key,
        env_key_instructions,
        experimental_bearer_token,
        auth,
        aws,
        wire_api,
        query_params,
        http_headers,
        env_http_headers,
        request_max_retries,
        stream_max_retries,
        stream_idle_timeout_ms,
        websocket_connect_timeout_ms,
        requires_openai_auth,
        supports_websockets,
        supports_standalone_web_search,
    } = provider;

    proto::ModelProvider {
        id: id.into(),
        name,
        base_url,
        env_key,
        env_key_instructions,
        experimental_bearer_token,
        auth: auth.map(model_provider_auth_to_proto),
        wire_api: proto_wire_api(wire_api).into(),
        query_params: query_params.map(proto_string_map),
        http_headers: http_headers.map(proto_string_map),
        env_http_headers: env_http_headers.map(proto_string_map),
        request_max_retries,
        stream_max_retries,
        stream_idle_timeout_ms,
        websocket_connect_timeout_ms,
        requires_openai_auth,
        supports_websockets,
        supports_standalone_web_search,
        aws: aws.map(|aws| proto::ModelProviderAwsAuthInfo {
            profile: aws.profile,
            region: aws.region,
        }),
    }
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

#[cfg(test)]
fn model_provider_auth_to_proto(auth: ModelProviderAuthInfo) -> proto::ModelProviderAuthInfo {
    let ModelProviderAuthInfo {
        command,
        args,
        timeout_ms,
        refresh_interval_ms,
        cwd,
    } = auth;

    proto::ModelProviderAuthInfo {
        command,
        args,
        timeout_ms: timeout_ms.get(),
        refresh_interval_ms,
        cwd: cwd.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
fn proto_string_map(values: HashMap<String, String>) -> proto::StringMap {
    proto::StringMap { values }
}

#[cfg(test)]
fn proto_wire_api(wire_api: WireApi) -> proto::WireApi {
    match wire_api {
        WireApi::Responses => proto::WireApi::Responses,
    }
}

fn parse_error(message: impl Into<String>) -> ThreadConfigLoadError {
    ThreadConfigLoadError::new(
        ThreadConfigLoadErrorCode::Parse,
        /*status_code*/ None,
        message.into(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::HashMap;
    use std::num::NonZeroU64;

    use codex_model_provider_info::ModelProviderInfo;
    use codex_model_provider_info::WireApi;
    use codex_protocol::config_types::ModelProviderAuthInfo;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tonic::Request;
    use tonic::Response;
    use tonic::Status;
    use tonic::transport::Server;

    use super::proto::thread_config_loader_server;
    use super::proto::thread_config_loader_server::ThreadConfigLoaderServer;
    use super::*;
    use crate::SessionThreadConfig;
    use crate::UserThreadConfig;

    struct TestServer {
        sources: Vec<proto::ThreadConfigSource>,
        expected_cwd: String,
        stall: bool,
    }

    impl TestServer {
        async fn load(
            &self,
            request: Request<proto::LoadThreadConfigRequest>,
        ) -> Result<Response<proto::LoadThreadConfigResponse>, Status> {
            if self.stall {
                std::future::pending::<()>().await;
            }
            assert_eq!(
                request.into_inner(),
                proto::LoadThreadConfigRequest {
                    thread_id: Some("thread-1".to_string()),
                    cwd: Some(self.expected_cwd.clone()),
                }
            );

            Ok(Response::new(proto::LoadThreadConfigResponse {
                sources: self.sources.clone(),
            }))
        }
    }

    impl thread_config_loader_server::ThreadConfigLoader for TestServer {
        fn load<'a, 'async_trait>(
            &'a self,
            request: Request<proto::LoadThreadConfigRequest>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Response<proto::LoadThreadConfigResponse>, Status>,
                    > + Send
                    + 'async_trait,
            >,
        >
        where
            'a: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(TestServer::load(self, request))
        }
    }

    #[tokio::test]
    async fn load_thread_config_calls_remote_service() {
        let cwd = workspace_dir().join("project");
        let expected_cwd = cwd.to_string_lossy().into_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_connections = Arc::clone(&connections);
        let server = tokio::spawn(async move {
            use futures::StreamExt;
            let incoming =
                tokio_stream::wrappers::TcpListenerStream::new(listener).inspect(move |_| {
                    server_connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                });
            Server::builder()
                .add_service(ThreadConfigLoaderServer::new(TestServer {
                    sources: proto_sources(),
                    expected_cwd,
                    stall: false,
                }))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        let loader = RemoteThreadConfigLoader::new(format!("http://{addr}"));
        let loaded = loader
            .load(ThreadConfigContext {
                thread_id: Some("thread-1".to_string()),
                cwd: Some(cwd.clone()),
            })
            .await;

        let loaded_again = ThreadConfigLoader::load(
            &loader.clone(),
            ThreadConfigContext {
                thread_id: Some("thread-1".to_string()),
                cwd: Some(cwd),
            },
        )
        .await;

        let _ = shutdown_tx.send(());
        server.await.expect("join server").expect("server");

        assert_eq!(loaded.expect("load thread config"), expected_sources());
        assert_eq!(
            loaded_again.expect("reload thread config"),
            expected_sources()
        );
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn load_thread_config_request_sets_timeout() {
        let request = load_thread_config_request(
            ThreadConfigContext::default(),
            REMOTE_THREAD_CONFIG_LOAD_TIMEOUT,
        );

        assert_eq!(
            request
                .metadata()
                .get("grpc-timeout")
                .and_then(|value| value.to_str().ok()),
            Some("5000000u")
        );
    }

    #[test]
    fn model_provider_proto_roundtrips_through_domain_type() {
        let mut expected = expected_provider();
        expected.auth = None;
        expected.supports_websockets = false;
        expected.supports_standalone_web_search = true;
        expected.aws = Some(ModelProviderAwsAuthInfo {
            profile: Some("test-profile".to_string()),
            region: Some("us-east-1".to_string()),
        });
        expected.validate().unwrap();
        let proto = model_provider_to_proto("local", expected.clone());
        let (id, actual) = model_provider_from_proto(proto).expect("model provider from proto");

        assert_eq!(id, "local");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn stalled_transport_times_out_through_loader_trait() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let loader =
            RemoteThreadConfigLoader::new(format!("http://{}", listener.local_addr().unwrap()));
        // Accept TCP but never complete the HTTP/2 handshake.
        let stalled_peer = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let result = tokio::time::timeout(
            REMOTE_THREAD_CONFIG_LOAD_TIMEOUT + Duration::from_secs(2),
            ThreadConfigLoader::load(&loader, ThreadConfigContext::default()),
        )
        .await;
        stalled_peer.abort();
        let error = result
            .expect("load must finish within its deadline")
            .unwrap_err();
        assert_eq!(error.code(), ThreadConfigLoadErrorCode::Timeout);
    }

    #[tokio::test]
    async fn stalled_rpc_is_reported_as_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let loader =
            RemoteThreadConfigLoader::new(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(ThreadConfigLoaderServer::new(TestServer {
                    sources: vec![],
                    expected_cwd: String::new(),
                    stall: true,
                }))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
        });
        let result = tokio::time::timeout(
            REMOTE_THREAD_CONFIG_LOAD_TIMEOUT + Duration::from_secs(2),
            ThreadConfigLoader::load(&loader, ThreadConfigContext::default()),
        )
        .await;
        server.abort();
        assert_eq!(
            result.expect("RPC deadline").unwrap_err().code(),
            ThreadConfigLoadErrorCode::Timeout
        );
    }

    #[test]
    fn duplicate_provider_ids_are_rejected_at_source_boundary() {
        let provider = model_provider_to_proto("local", expected_provider());
        let error = thread_config_source_from_proto(proto::ThreadConfigSource {
            source: Some(proto::thread_config_source::Source::Session(
                proto::SessionThreadConfig {
                    model_providers: vec![provider.clone(), provider],
                    ..Default::default()
                },
            )),
        })
        .unwrap_err();
        assert_eq!(error.code(), ThreadConfigLoadErrorCode::Parse);
        assert!(error.to_string().contains("duplicate model provider id"));
        assert!(error.to_string().contains("local"));
    }

    #[test]
    fn remote_errors_preserve_distinct_grpc_statuses() {
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::InvalidArgument,
            tonic::Code::FailedPrecondition,
        ] {
            let error = remote_status_to_error(Status::new(code, "test"));
            assert_eq!(error.code(), ThreadConfigLoadErrorCode::RequestFailed);
            assert_eq!(error.grpc_code(), Some(code));
            assert_eq!(error.status_code(), None);
        }
    }

    #[test]
    fn server_cancellation_is_not_a_timeout() {
        assert_eq!(
            remote_status_to_error(Status::cancelled("cancelled by server")).code(),
            ThreadConfigLoadErrorCode::RequestFailed
        );
        assert_eq!(
            remote_status_to_error(Status::deadline_exceeded("deadline")).code(),
            ThreadConfigLoadErrorCode::Timeout
        );
    }

    fn proto_sources() -> Vec<proto::ThreadConfigSource> {
        let workspace_cwd = workspace_dir().to_string_lossy().into_owned();
        vec![
            proto::ThreadConfigSource {
                source: Some(proto::thread_config_source::Source::Session(
                    proto::SessionThreadConfig {
                        model_provider: Some("local".to_string()),
                        model_providers: vec![proto::ModelProvider {
                            id: "local".to_string(),
                            name: "Local".to_string(),
                            base_url: Some("http://127.0.0.1:8061/api/codex".to_string()),
                            env_key: None,
                            env_key_instructions: None,
                            experimental_bearer_token: None,
                            auth: Some(proto::ModelProviderAuthInfo {
                                command: "token-helper".to_string(),
                                args: vec!["--json".to_string()],
                                timeout_ms: 5_000,
                                refresh_interval_ms: 300_000,
                                cwd: workspace_cwd,
                            }),
                            wire_api: proto::WireApi::Responses.into(),
                            query_params: Some(proto::StringMap {
                                values: HashMap::from([(
                                    "api-version".to_string(),
                                    "2026-04-16".to_string(),
                                )]),
                            }),
                            http_headers: Some(proto::StringMap {
                                values: HashMap::from([(
                                    "X-Test".to_string(),
                                    "enabled".to_string(),
                                )]),
                            }),
                            env_http_headers: Some(proto::StringMap {
                                values: HashMap::from([(
                                    "X-Env".to_string(),
                                    "LOCAL_HEADER".to_string(),
                                )]),
                            }),
                            request_max_retries: Some(7),
                            stream_max_retries: Some(8),
                            stream_idle_timeout_ms: Some(9_000),
                            websocket_connect_timeout_ms: Some(10_000),
                            requires_openai_auth: false,
                            supports_websockets: true,
                            supports_standalone_web_search: false,
                            aws: None,
                        }],
                        features: HashMap::from([
                            ("plugins".to_string(), false),
                            ("tools".to_string(), true),
                        ]),
                    },
                )),
            },
            proto::ThreadConfigSource {
                source: Some(proto::thread_config_source::Source::User(
                    proto::UserThreadConfig {},
                )),
            },
        ]
    }

    fn expected_sources() -> Vec<ThreadConfigSource> {
        vec![
            ThreadConfigSource::Session(SessionThreadConfig {
                model_provider: Some("local".to_string()),
                model_providers: HashMap::from([("local".to_string(), expected_provider())]),
                features: BTreeMap::from([
                    ("plugins".to_string(), false),
                    ("tools".to_string(), true),
                ]),
            }),
            ThreadConfigSource::User(UserThreadConfig::default()),
        ]
    }

    fn expected_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: "Local".to_string(),
            base_url: Some("http://127.0.0.1:8061/api/codex".to_string()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: Some(ModelProviderAuthInfo {
                command: "token-helper".to_string(),
                args: vec!["--json".to_string()],
                timeout_ms: NonZeroU64::new(5_000).expect("non-zero timeout"),
                refresh_interval_ms: 300_000,
                cwd: workspace_dir(),
            }),
            wire_api: WireApi::Responses,
            query_params: Some(HashMap::from([(
                "api-version".to_string(),
                "2026-04-16".to_string(),
            )])),
            http_headers: Some(HashMap::from([(
                "X-Test".to_string(),
                "enabled".to_string(),
            )])),
            env_http_headers: Some(HashMap::from([(
                "X-Env".to_string(),
                "LOCAL_HEADER".to_string(),
            )])),
            request_max_retries: Some(7),
            stream_max_retries: Some(8),
            stream_idle_timeout_ms: Some(9_000),
            websocket_connect_timeout_ms: Some(10_000),
            requires_openai_auth: false,
            supports_websockets: true,
            supports_standalone_web_search: false,
            aws: None,
        }
    }

    fn workspace_dir() -> AbsolutePathBuf {
        AbsolutePathBuf::current_dir()
            .expect("current dir")
            .join("workspace")
    }
}
